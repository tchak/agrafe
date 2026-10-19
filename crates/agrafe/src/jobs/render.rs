//! Rendering a blob's representations: fetch and decrypt the original once,
//! render its picture in a sandbox, fit the preview and the thumbnail from
//! it, store both encrypted.
//!
//! One job serves both representations and does whatever is `pending` on
//! the row. The picture is the file rendered as a whole, from the sources
//! its content type lists (`agrafe_render::plan`): the preview is the
//! picture in the preview box, and the thumbnail is the preview fitted
//! into the thumbnail box. The original is thus decoded once, the two
//! pictures agree, and a thumbnail regenerated on its own is made from the
//! stored preview without touching the original. A file with no picture,
//! or whose picture failed, gets a thumbnail from the plan's fallback when
//! there is one: a text sample, or the first page of a PDF.
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
use agrafe_render::{Fallback, Plan, Source, Verdict, helpers, plan};
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
    Failed,
    Unavailable,
    /// Turned off at creation: the thumbnail is rendered on its own.
    Disabled,
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
            (RepresentationState::Failed, true) => Preview::Failed,
            (RepresentationState::Unavailable, true) => Preview::Unavailable,
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
            Verdict::Rejected(_) => Preview::Failed,
            Verdict::Declined => Preview::Unavailable,
        };
        let state = state_of(&verdict);
        let (bytes, source) = verdict.rendered().unzip();
        self.finish(Representation::Preview, state, bytes, source)
            .await?;
        Ok(preview)
    }

    /// The thumbnail from `preview`: the preview fitted into the thumbnail
    /// box, or the fallback when there is no preview. A failed preview with
    /// nothing to fall back to leaves the thumbnail failed too: the file is
    /// at fault.
    async fn thumbnail(&mut self, preview: &Preview) -> Result<(), JobError> {
        let fit = Fit::thumbnail(&self.media().render);
        let output = self.path(THUMBNAIL);
        let verdict = match preview {
            Preview::Ready(path) => self.fit(path, Input::Raster, fit, &output).await?,
            Preview::Failed | Preview::Unavailable => self.thumbnail_fallback().await?,
            Preview::Disabled => self.thumbnail_from_picture().await?,
        };
        let state = match (preview, &verdict) {
            (Preview::Failed, Verdict::Declined) => RepresentationState::Failed,
            _ => state_of(&verdict),
        };
        self.finish(Representation::Thumbnail, state, verdict.rendered(), None)
            .await
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
            };
            match self.fit(&input, kind, fit, output).await? {
                Verdict::Rendered(bytes) => return Ok(Verdict::Rendered((bytes, from))),
                Verdict::Rejected(rejection) => {
                    tracing::warn!(?source, %rejection, "rendering failed");
                    rejected = Some(rejection);
                }
                Verdict::Declined => {}
            }
        }
        // Every source declined, as for a DOCX that is a real document,
        // unless one of them rejected the file.
        Ok(rejected.map_or(Verdict::Declined, Verdict::Rejected))
    }

    /// `input` fitted into `fit`, written to `output`.
    async fn fit(
        &self,
        input: &Path,
        kind: Input,
        fit: Fit,
        output: &Path,
    ) -> Result<Verdict<Vec<u8>>, JobError> {
        Ok(vips::to_webp(&self.media().render, input, kind, output, fit).await?)
    }

    /// A thumbnail for a blob whose preview is disabled: the picture fitted
    /// straight into the thumbnail box, then the fallback.
    async fn thumbnail_from_picture(&mut self) -> Result<Verdict<Vec<u8>>, JobError> {
        let fit = Fit::thumbnail(&self.media().render);
        let output = self.path(THUMBNAIL);
        match self.picture(fit, &output).await? {
            Verdict::Declined => self.thumbnail_fallback().await,
            verdict => Ok(verdict.map(|(bytes, _)| bytes)),
        }
    }

    /// A thumbnail for a file with no picture, from the plan's fallback:
    /// the first lines of a text file set on a page, or the first page of a
    /// PDF. Declines when there is none, and the thumbnail is the icon.
    async fn thumbnail_fallback(&mut self) -> Result<Verdict<Vec<u8>>, JobError> {
        match self.plan.fallback {
            Some(Fallback::TextSample) => self.text_sample().await,
            Some(Fallback::FirstPage) if !self.too_large() => {
                let fit = Fit::thumbnail(&self.media().render);
                let original = self.whole_original().await?;
                let verdict = self
                    .fit(&original, Input::Page, fit, &self.path(THUMBNAIL))
                    .await?;
                if let Verdict::Rejected(rejection) = &verdict {
                    tracing::warn!(%rejection, "rendering the first page failed");
                }
                Ok(verdict)
            }
            _ => Ok(Verdict::Declined),
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
