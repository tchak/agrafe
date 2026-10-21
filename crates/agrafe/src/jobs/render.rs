//! Rendering a blob's representations: fetch and decrypt the original once,
//! render its picture in a sandbox, fit the preview and the thumbnail from
//! it, store both encrypted.
//!
//! One job serves both representations and does whatever is `pending` on
//! the row. The picture is the file rendered as a whole, from the sources
//! its content type lists (`agrafe_render::plan`), for a GeoJSON or GPX
//! file its map, drawn by a helper over a background fetched from a
//! configured provider; the preview is the
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
//! A file the renderer rejects, or that crashes a helper, leaves the
//! representation `failed` and the job done: trying the same bytes again
//! would end the same way. Only failures of ours (store, database, a
//! missing renderer) fail the job. `agrafe_render::probe` runs the same
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
use crate::blob::{Blob, BlobState, PreviewSource, Representation, RepresentationState};
use crate::blob::{repo, representation};
use crate::config::MediaConfig;

/// The preview's file in the work directory.
const PREVIEW: &str = "preview.webp";
/// The thumbnail's file in the work directory.
const THUMBNAIL: &str = "thumbnail.webp";

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    if job.kind != JobKind::Render {
        return Err(JobError::Unhandled(job.kind));
    }
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    let Some(blob) = repo::find(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to render");
        return Ok(());
    };
    if blob.state != BlobState::Ready {
        tracing::info!(state = %blob.state, "blob is not ready, nothing to render");
        return Ok(());
    }
    let pending = |kind| blob.representation_state(kind) == RepresentationState::Pending;
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
    /// A WebP in the work directory, rendered now or decrypted from the
    /// store.
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
            Rejection::Unsupported | Rejection::Failed { .. } | Rejection::Output(_) => {
                NoPicture::Rejected
            }
        }
    }

    /// The stored verdict of a preview rendered by an earlier job, which
    /// does not say whether a rejection was a timeout.
    fn stored(state: RepresentationState) -> Self {
        match state {
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
    blob: &'a Blob,
    plan: Plan,
    stored: Stored,
    workdir: tempfile::TempDir,
    /// The whole original, decrypted on first use.
    original: Option<PathBuf>,
}

impl<'a> Render<'a> {
    fn new(state: &'a Agrafe, blob: &'a Blob) -> Result<Self, JobError> {
        Ok(Self {
            state,
            blob,
            plan: plan(&blob.content_type),
            stored: Stored::from_blob(blob)?,
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

    /// The preview the thumbnail derives from: rendered now when it is
    /// pending, or when the thumbnail needs one and the stored one is from
    /// another variant version or gone. A verdict from this version stands.
    async fn preview(&mut self, pending: bool) -> Result<Preview, JobError> {
        let blob = self.blob;
        if pending {
            return self.fresh_preview().await;
        }
        let current = blob.preview_variant_version == Some(self.media().variant_version);
        Ok(match (blob.preview_state, current) {
            (RepresentationState::Disabled, _) => Preview::Disabled,
            (RepresentationState::Ready, true) => match self.stored_preview().await? {
                Some(path) => Preview::Ready(path),
                None => return self.fresh_preview().await,
            },
            (state @ (RepresentationState::Failed | RepresentationState::Unavailable), true) => {
                Preview::Missing(NoPicture::stored(state))
            }
            _ => return self.fresh_preview().await,
        })
    }

    /// Render the picture into the preview box, record the outcome, and
    /// leave the WebP in the work directory.
    async fn fresh_preview(&mut self) -> Result<Preview, JobError> {
        let output = self.path(PREVIEW);
        let verdict = self
            .picture(Fit::preview(&self.media().render), &output)
            .await?;
        let preview = match &verdict {
            Verdict::Rendered(_) => Preview::Ready(output),
            Verdict::Rejected(rejection) => Preview::Missing(NoPicture::rejected(rejection)),
            Verdict::Declined => Preview::Missing(NoPicture::Declined),
        };
        let state = state_of(&verdict);
        let (bytes, source) = verdict.rendered().unzip();
        self.finish(Representation::Preview, state, bytes, source)
            .await?;
        Ok(preview)
    }

    /// The thumbnail from `preview`: the preview fitted into the thumbnail
    /// box, the picture fitted straight into it when the preview is
    /// disabled, or else whatever the file gets without a picture.
    async fn thumbnail(&mut self, preview: &Preview) -> Result<(), JobError> {
        let fit = Fit::thumbnail(&self.media().render);
        let output = self.path(THUMBNAIL);
        let (state, bytes) = match preview {
            Preview::Ready(path) => {
                let verdict = self.fit(path, Input::Raster, fit, &output).await?;
                (state_of(&verdict), verdict.rendered())
            }
            Preview::Missing(why) => self.thumbnail_without_picture(*why).await?,
            Preview::Disabled => match self.picture(fit, &output).await? {
                Verdict::Rendered((bytes, _)) => (RepresentationState::Ready, Some(bytes)),
                Verdict::Rejected(rejection) => {
                    self.thumbnail_without_picture(NoPicture::rejected(&rejection))
                        .await?
                }
                Verdict::Declined => self.thumbnail_without_picture(NoPicture::Declined).await?,
            },
        };
        self.finish(Representation::Thumbnail, state, bytes, None)
            .await
    }

    /// The thumbnail when the file has no picture: the plan's fallback, and
    /// the state it leaves, which `why` decides when the fallback yields
    /// nothing, see [`NoPicture`].
    async fn thumbnail_without_picture(
        &mut self,
        why: NoPicture,
    ) -> Result<(RepresentationState, Option<Vec<u8>>), JobError> {
        if why == NoPicture::TimedOut {
            tracing::info!("the picture timed out, not trying the fallback");
            return Ok((RepresentationState::Failed, None));
        }
        let verdict = self.thumbnail_fallback().await?;
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
        let verdict = vips::to_webp(&self.media().render, input, kind, output, fit).await?;
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
    async fn thumbnail_fallback(&mut self) -> Result<Verdict<Vec<u8>>, JobError> {
        match self.plan.fallback {
            Some(Fallback::TextSample) => self.text_sample().await,
            Some(Fallback::FirstPage) if !self.too_large() => {
                let fit = Fit::thumbnail(&self.media().render);
                let original = self.whole_original().await?;
                self.fit(&original, Input::Page, fit, &self.path(THUMBNAIL))
                    .await
            }
            Some(Fallback::ConvertedFirstPage) if !self.too_large() => {
                self.converted_first_page().await
            }
            _ => Ok(Verdict::Declined),
        }
    }

    /// The first page of an office file, converted to a PDF by the
    /// configured LibreOffice and rendered like a PDF page. Declines, and
    /// the icon stands, when no converter is configured.
    async fn converted_first_page(&mut self) -> Result<Verdict<Vec<u8>>, JobError> {
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
            Verdict::Rendered(pdf) => {
                let fit = Fit::thumbnail(&media.render);
                self.fit(&pdf, Input::Page, fit, &self.path(THUMBNAIL))
                    .await
            }
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
    /// file however large it is.
    async fn text_sample(&self) -> Result<Verdict<Vec<u8>>, JobError> {
        let media = self.media();
        let sample = self.path("sample");
        self.download(&sample, 0..self.stored.size.min(TEXT_SAMPLE_LEN))
            .await?;
        let page = self.path("page.pgm");
        let fit = Fit::thumbnail(&media.render);
        match helpers::render_text(&media.render, &sample, &page, fit).await? {
            Verdict::Rendered(()) => {
                self.fit(&page, Input::Raster, fit, &self.path(THUMBNAIL))
                    .await
            }
            Verdict::Declined => Ok(Verdict::Declined),
            Verdict::Rejected(rejection) => {
                tracing::warn!(%rejection, "rendering the text sample failed");
                Ok(Verdict::Rejected(rejection))
            }
        }
    }

    /// Store what was rendered, if anything, and record the state on the
    /// row with the variant version it was built with.
    async fn finish(
        &self,
        kind: Representation,
        state: RepresentationState,
        bytes: Option<Vec<u8>>,
        source: Option<PreviewSource>,
    ) -> Result<(), JobError> {
        let rendered = match &bytes {
            Some(bytes) => {
                let rendered = representation::store(self.state, &self.stored, kind, bytes).await?;
                tracing::info!(?source, bytes = bytes.len(), "{kind} rendered");
                Some(rendered)
            }
            None => None,
        };
        repo::finish_representation(
            self.state.pool(),
            self.blob.id,
            kind,
            state,
            source,
            self.media().variant_version,
            rendered,
        )
        .await?;
        Ok(())
    }

    /// Whether the original is over `media.max_source_size`, in which case
    /// nothing decodes it.
    fn too_large(&self) -> bool {
        self.blob
            .size
            .is_some_and(|size| size as u64 > self.media().max_source_size.as_u64())
    }

    /// The stored preview, decrypted into the work directory; `None` when
    /// the object is gone.
    async fn stored_preview(&self) -> Result<Option<PathBuf>, JobError> {
        let loaded =
            representation::load(self.state, &self.stored, Representation::Preview).await?;
        let Some(bytes) = loaded else {
            tracing::warn!("the stored preview is missing, rendering it again");
            return Ok(None);
        };
        let path = self.path(PREVIEW);
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
