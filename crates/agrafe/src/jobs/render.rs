//! Rendering a blob's representations: fetch and decrypt the original once,
//! render its picture in a sandbox, fit the preview and the thumbnail from
//! it, store both encrypted.
//!
//! One job serves both representations and does whatever is `pending` on
//! the row. The picture is the file rendered as a whole, from the sources
//! its content type lists (`agrafe_render::plan`), for a GeoJSON, GPX,
//! KML, KMZ or GML file or a zipped shapefile its map, drawn by a helper
//! over a background fetched from a configured provider; the preview is the
//! picture in the preview box, and the thumbnail is the preview fitted
//! into the thumbnail box. The original is thus decoded once, the two
//! pictures agree, and a thumbnail regenerated on its own is made from the
//! stored preview without touching the original. A file with no picture,
//! or whose picture failed, gets a thumbnail from the plan's fallback when
//! there is one: a text sample, the first page of a PDF, or the first
//! page of an office file once LibreOffice, when one is configured, has
//! converted it to a PDF. A file that timed out a tool is not handed to
//! another, see [`NoPicture`].
//!
//! With `media.webp_fallback`, each picture stored is transcoded to WebP
//! and stored again beside itself, for the clients that cannot decode
//! JPEG XL; see [`Render::store_fallback`].
//!
//! A file the renderer rejects, or that crashes a helper, leaves the
//! representation `failed` and the job done: trying the same bytes again
//! would end the same way. Only failures of ours (store, database, a
//! missing renderer) fail the job, and an original that does not open, the
//! row or the object damaged, kills it at once (`ReadError::is_permanent`):
//! no renderer has seen those bytes, so that is not the file's verdict.
//! `agrafe_render::probe` runs the same
//! tools on known files before a worker starts, so a broken deployment
//! fails there.

use std::ops::Range;
use std::path::{Path, PathBuf};

use agrafe_render::vips::{self, Fit, Input};
use agrafe_render::{Fallback, Plan, Rejection, Source, Verdict, geo, helpers, office, plan};
use agrafe_types::text::SAMPLE_LEN as TEXT_SAMPLE_LEN;
use tokio::io::AsyncWriteExt;

use super::{Job, JobError, JobKind};
use crate::app::Agrafe;
use crate::blob::read::{Reader, Stored};
use crate::blob::{
    BlobHead, BlobState, ImageFormat, PreviewSource, Representation, RepresentationState,
    WebpFallback,
};
use crate::blob::{key, repo, representation};
use crate::config::MediaConfig;
use crate::upload;

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    if job.kind != JobKind::Render {
        return Err(JobError::Unhandled(job.kind));
    }
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    // A running job holds no pending slot, so a request for the other
    // representation enqueues a twin, which would decode the original
    // beside this one, most often for what that one is already rendering.
    // It waits instead, and reads the row when the older one is done: what
    // that one did not get to is still `pending`, and nothing else would
    // pick it up.
    let lease = state.config().jobs.lease_timeout;
    if super::older_twin_running(state.pool(), job, lease).await? {
        return Err(JobError::Busy);
    }
    // The head, read from the row: what serving needs is what rendering
    // needs, the representations with it.
    let Some(blob) = repo::find_head(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to render");
        return Ok(());
    };
    if blob.state != BlobState::Ready {
        tracing::info!(state = %blob.state, "blob is not ready, nothing to render");
        return Ok(());
    }
    let pending = |kind| blob.representations.state(kind) == RepresentationState::Pending;
    if !pending(Representation::Preview) && !pending(Representation::Thumbnail) {
        tracing::info!("nothing is pending, nothing to render");
        return Ok(());
    }
    let mut render = Render::new(state, &blob)?;
    let preview = render.preview(pending(Representation::Preview)).await?;
    if pending(Representation::Thumbnail) {
        render.thumbnail(&preview).await?;
    }
    Ok(())
}

/// What the thumbnail is derived from.
enum Preview {
    /// A picture in the work directory, rendered now or decrypted from the
    /// store, in whatever format that one was built with: vips picks its
    /// loader by the bytes.
    Ready(PathBuf),
    /// There is none, and why: the thumbnail comes from the fallback.
    Missing(NoPicture),
    /// Turned off at creation: the thumbnail is rendered on its own.
    Disabled,
}

/// Why the picture is missing, which decides what the thumbnail may do
/// without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoPicture {
    /// Every source declined, or the file is too large: the fallback, else
    /// the icon.
    Declined,
    /// A source rejected the file: the fallback, else `failed`, since the
    /// file is at fault and the icon would say otherwise.
    Rejected,
    /// A tool ran out of time on the file: `failed`, without handing the
    /// same bytes to a second tool for as long again.
    TimedOut,
}

impl NoPicture {
    fn rejected(rejection: &Rejection) -> Self {
        match rejection {
            Rejection::TimedOut => NoPicture::TimedOut,
            Rejection::Unsupported
            | Rejection::Failed { .. }
            | Rejection::TooLarge
            | Rejection::Output(_) => NoPicture::Rejected,
        }
    }

    /// The stored verdict of a preview rendered by an earlier job: the same
    /// bytes get the same thumbnail, whichever was asked for first.
    fn stored(state: RepresentationState, timed_out: bool) -> Self {
        match state {
            RepresentationState::Failed if timed_out => NoPicture::TimedOut,
            RepresentationState::Failed => NoPicture::Rejected,
            _ => NoPicture::Declined,
        }
    }
}

/// The state a verdict leaves a representation in.
fn state_of<T>(verdict: &Verdict<T>) -> RepresentationState {
    match verdict {
        Verdict::Rendered(_) => RepresentationState::Ready,
        Verdict::Rejected(_) => RepresentationState::Failed,
        Verdict::Declined => RepresentationState::Unavailable,
    }
}

/// One job's rendering: the blob, its keys, its plan, and a work directory
/// removed when the job is done.
struct Render<'a> {
    state: &'a Agrafe,
    blob: &'a BlobHead,
    plan: Plan,
    stored: Stored,
    workdir: tempfile::TempDir,
    /// The whole original, decrypted on first use.
    original: Option<PathBuf>,
}

impl<'a> Render<'a> {
    fn new(state: &'a Agrafe, blob: &'a BlobHead) -> Result<Self, JobError> {
        Ok(Self {
            state,
            blob,
            plan: plan(&blob.content_type),
            stored: Stored::from_head(blob)?,
            workdir: agrafe_render::workdir(state.config().media.tmp_dir.as_deref())?,
            original: None,
        })
    }

    fn media(&self) -> &'a MediaConfig {
        let state: &'a Agrafe = self.state;
        &state.config().media
    }

    fn path(&self, name: &str) -> PathBuf {
        self.workdir.path().join(name)
    }

    /// Where the `kind` picture is rendered to, named for the format `fit`
    /// encodes it in, which is what vips picks its saver by.
    fn output(&self, kind: Representation, fit: Fit) -> PathBuf {
        self.path(&fit.file_name(kind.as_str()))
    }

    /// The box, format and quality of `kind`.
    fn fit_for(&self, kind: Representation) -> Fit {
        match kind {
            Representation::Thumbnail => Fit::thumbnail(&self.media().render),
            Representation::Preview => Fit::preview(&self.media().render),
        }
    }

    /// The preview the thumbnail derives from: rendered now when it is
    /// pending, or when the thumbnail needs one and the stored one is from
    /// another variant version or gone. A verdict from this version stands.
    async fn preview(&mut self, pending: bool) -> Result<Preview, JobError> {
        if pending {
            return self.fresh_preview().await;
        }
        let Some(row) = self.blob.representations.get(Representation::Preview) else {
            return Ok(Preview::Disabled);
        };
        let current = row.version == Some(self.media().variant_version);
        Ok(match (row.state, current) {
            (RepresentationState::Ready, true) => match self.stored_preview().await? {
                Some(path) => Preview::Ready(path),
                None => return self.fresh_preview().await,
            },
            (state @ (RepresentationState::Failed | RepresentationState::Unavailable), true) => {
                Preview::Missing(NoPicture::stored(state, row.timed_out))
            }
            _ => return self.fresh_preview().await,
        })
    }

    /// Render the picture into the preview box, record the outcome, and
    /// leave the picture in the work directory.
    async fn fresh_preview(&mut self) -> Result<Preview, JobError> {
        let fit = Fit::preview(&self.media().render);
        let output = self.output(Representation::Preview, fit);
        let verdict = self.picture(fit, &output).await?;
        let preview = match &verdict {
            Verdict::Rendered(_) => Preview::Ready(output.clone()),
            Verdict::Rejected(rejection) => Preview::Missing(NoPicture::rejected(rejection)),
            Verdict::Declined => Preview::Missing(NoPicture::Declined),
        };
        let state = state_of(&verdict);
        let timed_out = matches!(preview, Preview::Missing(NoPicture::TimedOut));
        let (bytes, source) = verdict.rendered().unzip();
        self.finish(
            Representation::Preview,
            state,
            bytes,
            &output,
            source,
            timed_out,
        )
        .await?;
        Ok(preview)
    }

    /// The thumbnail from `preview`: the preview fitted into the thumbnail
    /// box, the picture fitted straight into it when the preview is
    /// disabled, or else whatever the file gets without a picture.
    async fn thumbnail(&mut self, preview: &Preview) -> Result<(), JobError> {
        let fit = Fit::thumbnail(&self.media().render);
        let output = self.output(Representation::Thumbnail, fit);
        let (state, bytes) = match preview {
            Preview::Ready(path) => {
                let verdict = self.fit(path, Input::Raster, fit, &output).await?;
                (state_of(&verdict), verdict.rendered())
            }
            Preview::Missing(why) => self.thumbnail_without_picture(*why, fit, &output).await?,
            Preview::Disabled => match self.picture(fit, &output).await? {
                Verdict::Rendered((bytes, _)) => (RepresentationState::Ready, Some(bytes)),
                Verdict::Rejected(rejection) => {
                    self.thumbnail_without_picture(NoPicture::rejected(&rejection), fit, &output)
                        .await?
                }
                Verdict::Declined => {
                    self.thumbnail_without_picture(NoPicture::Declined, fit, &output)
                        .await?
                }
            },
        };
        self.finish(
            Representation::Thumbnail,
            state,
            bytes,
            &output,
            None,
            false,
        )
        .await
    }

    /// The thumbnail when the file has no picture: the plan's fallback,
    /// fitted into `fit` at `output`, and the state it leaves, which `why`
    /// decides when the fallback yields nothing, see [`NoPicture`].
    async fn thumbnail_without_picture(
        &mut self,
        why: NoPicture,
        fit: Fit,
        output: &Path,
    ) -> Result<(RepresentationState, Option<Vec<u8>>), JobError> {
        if why == NoPicture::TimedOut {
            tracing::info!("the picture timed out, not trying the fallback");
            return Ok((RepresentationState::Failed, None));
        }
        let verdict = self.thumbnail_fallback(fit, output).await?;
        let state = match (why, &verdict) {
            (NoPicture::Rejected, Verdict::Declined) => RepresentationState::Failed,
            _ => state_of(&verdict),
        };
        Ok((state, verdict.rendered()))
    }

    /// The picture, from the first source of the plan that yields one,
    /// fitted into `fit` and written to `output`.
    async fn picture(
        &mut self,
        fit: Fit,
        output: &Path,
    ) -> Result<Verdict<(Vec<u8>, PreviewSource)>, JobError> {
        let media = self.media();
        let content_type = &self.blob.content_type;
        let too_large = self.too_large();
        let sources = self.plan.picture;
        if too_large || sources.is_empty() {
            tracing::info!(%content_type, too_large, "nothing to render a picture from");
            return Ok(Verdict::Declined);
        }
        let mut rejected = None;
        for source in sources {
            let (input, kind, from) = match source {
                Source::Image => (
                    self.whole_original().await?,
                    Input::for_image(content_type),
                    PreviewSource::Image,
                ),
                Source::EmbeddedImage => {
                    let whole = self.whole_original().await?;
                    let embedded = self.path("embedded");
                    match helpers::extract(&media.render, content_type, &whole, &embedded).await? {
                        Verdict::Rendered(()) => {}
                        Verdict::Declined => continue,
                        Verdict::Rejected(rejection) => {
                            tracing::warn!(?source, %rejection, "extraction failed");
                            rejected = Some(rejection);
                            continue;
                        }
                    }
                    (embedded, Input::Raster, PreviewSource::EmbeddedImage)
                }
                Source::Map(format) => {
                    let format = *format;
                    let whole = self.whole_original().await?;
                    let maps = self.state.maps();
                    match geo::render_map(
                        &media.render,
                        maps,
                        format,
                        &whole,
                        self.workdir.path(),
                        fit,
                    )
                    .await?
                    {
                        Verdict::Rendered(map) => (map, Input::Raster, PreviewSource::from(format)),
                        Verdict::Declined => continue,
                        Verdict::Rejected(rejection) => {
                            tracing::warn!(?source, %rejection, "drawing the map failed");
                            rejected = Some(rejection);
                            continue;
                        }
                    }
                }
            };
            match self.fit(&input, kind, fit, output).await? {
                Verdict::Rendered(bytes) => return Ok(Verdict::Rendered((bytes, from))),
                Verdict::Rejected(rejection) => rejected = Some(rejection),
                Verdict::Declined => {}
            }
        }
        // Every source declined, as for a DOCX that is a real document,
        // unless one of them rejected the file.
        Ok(rejected.map_or(Verdict::Declined, Verdict::Rejected))
    }

    /// `input` fitted into `fit`, written to `output`. A rejection is
    /// logged here, once, whatever is being fitted.
    async fn fit(
        &self,
        input: &Path,
        kind: Input,
        fit: Fit,
        output: &Path,
    ) -> Result<Verdict<Vec<u8>>, JobError> {
        let verdict = vips::render(&self.media().render, input, kind, output, fit).await?;
        if let Verdict::Rejected(rejection) = &verdict {
            let input = input.file_name().unwrap_or_default().to_string_lossy();
            tracing::warn!(%input, ?kind, %rejection, "rendering failed");
        }
        Ok(verdict)
    }

    /// A thumbnail for a file with no picture, from the plan's fallback:
    /// the first lines of a text file set on a page, the first page of a
    /// PDF, or the first page of an office file converted to one. Declines
    /// when there is none, and the thumbnail is the icon.
    async fn thumbnail_fallback(
        &mut self,
        fit: Fit,
        output: &Path,
    ) -> Result<Verdict<Vec<u8>>, JobError> {
        match self.plan.fallback {
            Some(Fallback::TextSample) => self.text_sample(fit, output).await,
            Some(Fallback::FirstPage) if !self.too_large() => {
                let original = self.whole_original().await?;
                self.fit(&original, Input::Page, fit, output).await
            }
            Some(Fallback::ConvertedFirstPage) if !self.too_large() => {
                self.converted_first_page(fit, output).await
            }
            _ => Ok(Verdict::Declined),
        }
    }

    /// The first page of an office file, converted to a PDF by the
    /// configured LibreOffice and rendered like a PDF page into `fit` at
    /// `output`. Declines, and the icon stands, when no converter is
    /// configured.
    async fn converted_first_page(
        &mut self,
        fit: Fit,
        output: &Path,
    ) -> Result<Verdict<Vec<u8>>, JobError> {
        let media = self.media();
        let blob = self.blob;
        // The converter declines without a command too; asked here, before
        // the original is fetched and decrypted for nothing.
        if media.render.office.command.is_none() {
            tracing::info!("no office converter configured, nothing to render a page from");
            return Ok(Verdict::Declined);
        }
        let original = self.whole_original().await?;
        let converted = office::convert(
            &media.render,
            &blob.content_type,
            &original,
            self.workdir.path(),
        )
        .await?;
        match converted {
            Verdict::Rendered(pdf) => self.fit(&pdf, Input::Page, fit, output).await,
            Verdict::Declined => {
                tracing::info!("the office converter wrote no PDF");
                Ok(Verdict::Declined)
            }
            Verdict::Rejected(rejection) => {
                tracing::warn!(%rejection, "converting to PDF failed");
                Ok(Verdict::Rejected(rejection))
            }
        }
    }

    /// The first lines of a text file set on a page, from a sample of the
    /// file however large it is, fitted into `fit` at `output`.
    async fn text_sample(&self, fit: Fit, output: &Path) -> Result<Verdict<Vec<u8>>, JobError> {
        let media = self.media();
        let sample = self.path("sample");
        self.download(&sample, 0..self.stored.size.min(TEXT_SAMPLE_LEN))
            .await?;
        let page = self.path("page.pgm");
        match helpers::render_text(&media.render, &sample, &page, fit).await? {
            Verdict::Rendered(()) => self.fit(&page, Input::Raster, fit, output).await,
            Verdict::Declined => Ok(Verdict::Declined),
            Verdict::Rejected(rejection) => {
                tracing::warn!(%rejection, "rendering the text sample failed");
                Ok(Verdict::Rejected(rejection))
            }
        }
    }

    /// Store what was rendered, if anything, with its WebP fallback where
    /// `media.webp_fallback` asks for one, and record the state on the row
    /// with the variant version it was built with. `picture` is where
    /// `bytes` were rendered to, which the fallback is transcoded from. A
    /// fallback the row recorded from an earlier render and this one did
    /// not store, the setting turned off since, goes with the row's record
    /// of it.
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        kind: Representation,
        state: RepresentationState,
        bytes: Option<Vec<u8>>,
        picture: &Path,
        source: Option<PreviewSource>,
        timed_out: bool,
    ) -> Result<(), JobError> {
        let render = &self.media().render;
        let (rendered, fallback) = match &bytes {
            Some(bytes) => {
                let format = render.format;
                let rendered =
                    representation::store(self.state, &self.stored, kind, format, bytes).await?;
                let fallback = if render.webp_fallback && format != ImageFormat::Webp {
                    Some(self.store_fallback(kind, picture).await?)
                } else {
                    None
                };
                tracing::info!(
                    ?source,
                    %format,
                    bytes = bytes.len(),
                    fallback = fallback.map(|f| f.size),
                    "{kind} rendered"
                );
                (Some(rendered), fallback)
            }
            None => (None, None),
        };
        let recorded = repo::finish_representation(
            self.state.pool(),
            self.blob.id,
            kind,
            state,
            source,
            timed_out,
            self.media().variant_version,
            rendered,
            fallback,
        )
        .await?;
        // Deleted meanwhile: a purge that has already swept the prefix will
        // not come back for what was just written under it.
        if !recorded && rendered.is_some() {
            tracing::info!("the blob is no longer ready, dropping its {kind}");
            upload::delete_quietly(self.state, &key::representation(self.blob.id, kind)).await;
            if fallback.is_some() {
                upload::delete_quietly(self.state, &key::fallback(self.blob.id, kind)).await;
            }
        }
        // The row said a fallback stood in the bucket and now says none
        // does: the object goes too. Only then, so that a deployment without
        // the fallback never pays a delete per render.
        if recorded && fallback.is_none() && self.blob.representations.fallback(kind).is_some() {
            tracing::info!("no WebP fallback is wanted any more, dropping the {kind}'s");
            upload::delete_quietly(self.state, &key::fallback(self.blob.id, kind)).await;
        }
        Ok(())
    }

    /// The WebP fallback of the `kind` picture just rendered at `picture`:
    /// the picture, which fits its box already, transcoded by vips and
    /// stored at `<kind>.webp`. A second generation, as the thumbnail is of
    /// the preview, so that every path renders once and one path encodes
    /// the fallback. vips just wrote the picture, so a rejection here is
    /// not the file's: the job fails, and the probe has checked the saver.
    async fn store_fallback(
        &self,
        kind: Representation,
        picture: &Path,
    ) -> Result<WebpFallback, JobError> {
        let fit = self.fit_for(kind).webp_fallback();
        let output = self.output(kind, fit);
        let verdict =
            vips::render(&self.media().render, picture, Input::Raster, &output, fit).await?;
        let bytes = match verdict {
            Verdict::Rendered(bytes) => bytes,
            Verdict::Rejected(rejection) => return Err(JobError::Fallback { kind, rejection }),
            Verdict::Declined => unreachable!("vips never declines"),
        };
        Ok(representation::store_fallback(self.state, &self.stored, kind, &bytes).await?)
    }

    /// Whether the original is over `media.max_source_size`, in which case
    /// nothing decodes it.
    fn too_large(&self) -> bool {
        self.blob
            .size
            .is_some_and(|size| size as u64 > self.media().max_source_size.as_u64())
    }

    /// The stored preview, decrypted into the work directory; `None` when
    /// the object is gone or does not open, and the original can give it
    /// again. Named `preview`, with no extension: it is in the format it
    /// was built with, which vips tells by the bytes, and a stale name
    /// must not suggest otherwise.
    async fn stored_preview(&self) -> Result<Option<PathBuf>, JobError> {
        let loaded = representation::load(
            self.state,
            &self.stored,
            Representation::Preview,
            crate::blob::Object::Picture,
        )
        .await;
        let bytes = match loaded {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                tracing::warn!("the stored preview is missing, rendering it again");
                return Ok(None);
            }
            Err(err @ representation::Error::Corrupt { .. }) => {
                tracing::error!(error = %err, "rendering the stored preview again");
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        let path = self.path("preview");
        tokio::fs::write(&path, bytes).await?;
        Ok(Some(path))
    }

    /// The whole original, decrypted into the work directory on first use.
    async fn whole_original(&mut self) -> Result<PathBuf, JobError> {
        if let Some(path) = &self.original {
            return Ok(path.clone());
        }
        let path = self.path("original");
        self.download(&path, 0..self.stored.size).await?;
        self.original = Some(path.clone());
        Ok(path)
    }

    /// Decrypt the plaintext `span` of the original into `path`.
    async fn download(&self, path: &Path, span: Range<u64>) -> Result<(), JobError> {
        let mut reader = Reader::open(self.state, &self.stored, span).await?;
        let mut file = tokio::fs::File::create(path).await?;
        while let Some(plain) = reader.next_segment().await? {
            file.write_all(&plain).await?;
        }
        file.flush().await?;
        Ok(())
    }
}
