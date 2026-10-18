//! Rendering a blob's representations: fetch and decrypt the original once,
//! pick a source, rasterise the preview in a sandbox, derive the thumbnail
//! from it, store both encrypted.
//!
//! One job serves both representations and does whatever is `pending` on
//! the row. The thumbnail is the preview fitted into the thumbnail box: the
//! original is decoded once, the two pictures agree, and a thumbnail
//! regenerated on its own is made from the stored preview without touching
//! the original. Files with no preview get a thumbnail of their own when
//! there is a fallback for them: a text sample, or the first page of a
//! PDF.
//!
//! A file the renderer rejects, or that crashes a helper, leaves the
//! representation `failed` and the job done: trying the same bytes again
//! would end the same way. Only failures of ours (store, database, a
//! missing renderer) fail the job. `agrafe_render::probe` runs the same
//! tools on known files before a worker starts, so a broken deployment
//! fails there.

use std::ops::Range;
use std::path::{Path, PathBuf};

use agrafe_render::helpers::{self, Helper};
use agrafe_render::vips::{self, Fit, Input};
use agrafe_render::{Fallback, Source, sources, thumbnail_fallback};
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

    // The preview comes first: rendered when it is pending, or when the
    // thumbnail needs one and the stored one is from another version or
    // gone. A verdict from this version stands.
    let current = blob.preview_variant_version == Some(render.media().variant_version);
    let preview = match (
        pending(Representation::Preview),
        blob.preview_state,
        current,
    ) {
        (false, RepresentationState::Ready, true) => match render.stored_preview().await? {
            Some(path) => Preview::Ready(path),
            None => render.preview().await?,
        },
        (false, RepresentationState::Failed, true) => Preview::Failed,
        (false, RepresentationState::Unavailable, true) => Preview::Unavailable,
        (false, RepresentationState::Disabled, _) => Preview::Disabled,
        _ => render.preview().await?,
    };

    if pending(Representation::Thumbnail) {
        let verdict = match &preview {
            Preview::Ready(path) => render.thumbnail_webp(path, Input::Raster).await?,
            Preview::Failed => Verdict::Rejected,
            Preview::Unavailable => render.thumbnail_fallback().await?,
            Preview::Disabled => render.thumbnail_alone().await?,
        };
        render
            .finish(
                Representation::Thumbnail,
                verdict.map(|bytes| (bytes, None)),
            )
            .await?;
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

/// How an attempt at a representation ended.
enum Verdict<T> {
    Rendered(T),
    /// The file's fault: the renderer or a helper rejected it, or crashed.
    Rejected,
    /// Nothing to render from.
    Declined,
}

impl<T> Verdict<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Verdict<U> {
        match self {
            Verdict::Rendered(value) => Verdict::Rendered(f(value)),
            Verdict::Rejected => Verdict::Rejected,
            Verdict::Declined => Verdict::Declined,
        }
    }
}

/// One job's rendering: the blob, its keys, and a work directory removed
/// when the job is done.
struct Render<'a> {
    state: &'a Agrafe,
    blob: &'a Blob,
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

    /// Render the preview from the first source that can, record the
    /// outcome, and leave the WebP in the work directory.
    async fn preview(&mut self) -> Result<Preview, JobError> {
        let verdict = self
            .render_preview(Fit::preview(&self.media().render))
            .await?;
        let preview = match &verdict {
            Verdict::Rendered(_) => Preview::Ready(self.path(PREVIEW)),
            Verdict::Rejected => Preview::Failed,
            Verdict::Declined => Preview::Unavailable,
        };
        self.finish(
            Representation::Preview,
            verdict.map(|(bytes, source)| (bytes, Some(source))),
        )
        .await?;
        Ok(preview)
    }

    /// The preview sources tried in order, into `fit`: the preview box, or
    /// the thumbnail box when the preview is disabled and the thumbnail is
    /// rendered on its own.
    async fn render_preview(
        &mut self,
        fit: Fit,
    ) -> Result<Verdict<(Vec<u8>, PreviewSource)>, JobError> {
        let media = self.media();
        let content_type = &self.blob.content_type;
        let too_large = self.too_large();
        let sources = sources(content_type);
        if too_large || sources.is_empty() {
            tracing::info!(%content_type, too_large, "nothing to render a preview from");
            return Ok(Verdict::Declined);
        }
        let output = self.path(PREVIEW);
        let mut rejected = false;
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
                        Helper::Produced => {}
                        Helper::Declined => continue,
                        Helper::Failed => {
                            rejected = true;
                            continue;
                        }
                    }
                    (embedded, Input::Raster, PreviewSource::EmbeddedImage)
                }
            };
            match vips::to_webp(&media.render, &input, kind, &output, fit).await {
                Ok(bytes) => return Ok(Verdict::Rendered((bytes, from))),
                Err(err) if err.is_input_fault() => {
                    tracing::warn!(source = ?source, error = %err, "rendering failed");
                    rejected = true;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(if rejected {
            Verdict::Rejected
        } else {
            // Every source declined, as for a DOCX that is a real document.
            Verdict::Declined
        })
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

    /// Whether the original is over `media.max_source_size`, in which case
    /// nothing decodes it.
    fn too_large(&self) -> bool {
        self.blob
            .size
            .is_some_and(|size| size as u64 > self.media().max_source_size.as_u64())
    }

    /// `input` fitted into the thumbnail box.
    async fn thumbnail_webp(
        &self,
        input: &Path,
        kind: Input,
    ) -> Result<Verdict<Vec<u8>>, JobError> {
        let media = self.media();
        let output = self.path("thumbnail.webp");
        let render = &media.render;
        match vips::to_webp(render, input, kind, &output, Fit::thumbnail(render)).await {
            Ok(bytes) => Ok(Verdict::Rendered(bytes)),
            Err(err) if err.is_input_fault() => {
                tracing::warn!(error = %err, "thumbnail rendering failed");
                Ok(Verdict::Rejected)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// A thumbnail for a blob whose preview is disabled: the preview
    /// sources rendered straight into the thumbnail box, then the fallback.
    async fn thumbnail_alone(&mut self) -> Result<Verdict<Vec<u8>>, JobError> {
        let fit = Fit::thumbnail(&self.media().render);
        match self.render_preview(fit).await? {
            Verdict::Rendered((bytes, _)) => Ok(Verdict::Rendered(bytes)),
            Verdict::Rejected => Ok(Verdict::Rejected),
            Verdict::Declined => self.thumbnail_fallback().await,
        }
    }

    /// A thumbnail for a file with no preview, rendered from the original:
    /// the first lines of a text file set on a page, or the first page of a
    /// PDF. Anything else declines, and gets the icon.
    async fn thumbnail_fallback(&mut self) -> Result<Verdict<Vec<u8>>, JobError> {
        match thumbnail_fallback(&self.blob.content_type) {
            Some(Fallback::TextSample) => self.text_sample().await,
            Some(Fallback::FirstPage) if !self.too_large() => {
                let original = self.whole_original().await?;
                self.thumbnail_webp(&original, Input::Page).await
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
        let render = &media.render;
        match helpers::render_text(render, &sample, &page, Fit::thumbnail(render)).await? {
            Helper::Produced => self.thumbnail_webp(&page, Input::Raster).await,
            Helper::Declined => Ok(Verdict::Declined),
            Helper::Failed => Ok(Verdict::Rejected),
        }
    }

    /// Store a rendered representation and record the verdict on the row,
    /// with the variant version it was built with.
    async fn finish(
        &self,
        kind: Representation,
        verdict: Verdict<(Vec<u8>, Option<PreviewSource>)>,
    ) -> Result<(), JobError> {
        let (rep_state, source) = match verdict {
            Verdict::Rendered((bytes, source)) => {
                representation::store(self.state, &self.stored, kind, &bytes).await?;
                tracing::info!(source = ?source, bytes = bytes.len(), "{kind} rendered");
                (RepresentationState::Ready, source)
            }
            Verdict::Rejected => (RepresentationState::Failed, None),
            Verdict::Declined => (RepresentationState::Unavailable, None),
        };
        repo::finish_representation(
            self.state.pool(),
            self.blob.id,
            kind,
            rep_state,
            source,
            self.media().variant_version,
        )
        .await?;
        Ok(())
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
