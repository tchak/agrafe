//! Rendering a blob's representations: fetch and decrypt the original once,
//! pick a source, rasterise the preview in a sandbox, derive the thumbnail
//! from it, store both encrypted.
//!
//! One job serves both representations and does whatever is `pending` on
//! the row. The thumbnail is the preview fitted into the thumbnail box: the
//! original is decoded once, the two pictures agree, and a thumbnail
//! regenerated on its own is made from the stored preview without touching
//! the original. Files with no preview get a thumbnail of their own when
//! they are text.
//!
//! A file the renderer rejects, or that crashes a helper, leaves the
//! representation `failed` and the job done: trying the same bytes again
//! would end the same way. Only failures of ours (store, database, a
//! missing renderer) fail the job. `probe` runs the same tools on known
//! files before a worker starts, so a broken deployment fails there.

use std::ops::Range;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use super::{Job, JobError, JobKind};
use crate::app::AppState;
use crate::blob::read::{Reader, Stored};
use crate::blob::{Blob, BlobState, PreviewSource, Representation, RepresentationState};
use crate::blob::{repo, representation};
use crate::config::MediaConfig;
use crate::media::render::{self, Fit, Input};
use crate::media::sandbox::{self, Outcome};
use crate::media::{self, Source, text};

/// The preview's file in the work directory.
const PREVIEW: &str = "preview.webp";

pub async fn run(state: &AppState, job: &Job) -> Result<(), JobError> {
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
        _ => render.preview().await?,
    };

    if pending(Representation::Thumbnail) {
        let verdict = match &preview {
            Preview::Ready(path) => render.thumbnail_webp(path).await?,
            Preview::Failed => Verdict::Rejected,
            Preview::Unavailable => render.text_sample().await?,
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
    state: &'a AppState,
    blob: &'a Blob,
    stored: Stored,
    workdir: tempfile::TempDir,
    /// The whole original, decrypted on first use.
    original: Option<PathBuf>,
}

impl<'a> Render<'a> {
    fn new(state: &'a AppState, blob: &'a Blob) -> Result<Self, JobError> {
        Ok(Self {
            state,
            blob,
            stored: Stored::from_blob(blob)?,
            workdir: workdir(&state.config().media)?,
            original: None,
        })
    }

    fn media(&self) -> &'a MediaConfig {
        let state: &'a AppState = self.state;
        &state.config().media
    }

    fn path(&self, name: &str) -> PathBuf {
        self.workdir.path().join(name)
    }

    /// Render the preview from the first source that can, record the
    /// outcome, and leave the WebP in the work directory.
    async fn preview(&mut self) -> Result<Preview, JobError> {
        let verdict = self.render_preview().await?;
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

    async fn render_preview(&mut self) -> Result<Verdict<(Vec<u8>, PreviewSource)>, JobError> {
        let media = self.media();
        let content_type = &self.blob.content_type;
        let too_large = self
            .blob
            .size
            .is_some_and(|size| size as u64 > media.max_source_size.as_u64());
        let sources = media::sources(content_type);
        if too_large || sources.is_empty() {
            tracing::info!(%content_type, too_large, "nothing to render a preview from");
            return Ok(Verdict::Declined);
        }
        let fit = Fit::preview(media);
        let output = self.path(PREVIEW);
        let mut rejected = false;
        for source in sources {
            let (input, kind, from) = match source {
                Source::Image => (
                    self.whole_original().await?,
                    Input::for_image(content_type),
                    PreviewSource::Image,
                ),
                Source::FirstPage => (
                    self.whole_original().await?,
                    Input::Page,
                    PreviewSource::FirstPage,
                ),
                Source::EmbeddedImage => {
                    let whole = self.whole_original().await?;
                    let embedded = self.path("embedded");
                    match extract(media, content_type, &whole, &embedded).await? {
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
            match render::to_webp(media, &input, kind, &output, fit).await {
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
        let loaded = representation::load(self.state, self.blob, Representation::Preview).await?;
        let Some(bytes) = loaded else {
            tracing::warn!("the stored preview is missing, rendering it again");
            return Ok(None);
        };
        let path = self.path(PREVIEW);
        tokio::fs::write(&path, bytes).await?;
        Ok(Some(path))
    }

    /// `input` fitted into the thumbnail box, never enlarged.
    async fn thumbnail_webp(&self, input: &Path) -> Result<Verdict<Vec<u8>>, JobError> {
        let media = self.media();
        let output = self.path("thumbnail.webp");
        match render::to_webp(media, input, Input::Raster, &output, Fit::thumbnail(media)).await {
            Ok(bytes) => Ok(Verdict::Rendered(bytes)),
            Err(err) if err.is_input_fault() => {
                tracing::warn!(error = %err, "thumbnail rendering failed");
                Ok(Verdict::Rejected)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// A thumbnail for a file with no preview: the first lines of a text
    /// file set on a page. Anything else declines, and gets the icon.
    async fn text_sample(&self) -> Result<Verdict<Vec<u8>>, JobError> {
        if !media::text_sample(&self.blob.content_type) {
            return Ok(Verdict::Declined);
        }
        let media = self.media();
        let sample = self.path("sample");
        self.download(&sample, 0..self.stored.size.min(text::SAMPLE_LEN))
            .await?;
        let page = self.path("page.pgm");
        match render_text(media, &sample, &page, Fit::thumbnail(media)).await? {
            Helper::Produced => self.thumbnail_webp(&page).await,
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
                representation::store(self.state, self.blob, kind, &bytes).await?;
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
        let mut reader = Reader::open(self.state, self.blob, &self.stored, span).await?;
        let mut file = tokio::fs::File::create(path).await?;
        while let Some(plain) = reader.next_segment().await? {
            file.write_all(&plain).await?;
        }
        file.flush().await?;
        Ok(())
    }
}

/// A fresh directory for one render's files, under `media.tmp_dir`.
fn workdir(media: &MediaConfig) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("agrafe-")
        .tempdir_in(media.tmp_dir.clone().unwrap_or_else(std::env::temp_dir))
}

/// How a helper run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Helper {
    /// It wrote its output.
    Produced,
    /// It found nothing to do: no embedded image, a sample that is not text.
    Declined,
    /// It crashed or ran out of time, which the file takes the blame for.
    Failed,
}

/// Run the `agrafe extract` helper on `original`, writing the embedded
/// image, if there is one, to `output`.
async fn extract(
    media: &MediaConfig,
    content_type: &str,
    original: &Path,
    output: &Path,
) -> Result<Helper, JobError> {
    let mut command = helper(media)?;
    command
        .arg("extract")
        .arg("--input")
        .arg(original)
        .arg("--output")
        .arg(output)
        .arg("--content-type")
        .arg(content_type)
        .arg("--min-coverage")
        .arg(media.embedded_image.min_coverage.to_string())
        .arg("--max-text-chars")
        .arg(media.embedded_image.max_text_chars.to_string())
        .arg("--max-output")
        .arg(media.sandbox.max_output_size.as_u64().to_string());
    run_helper(media, command, output, "extraction").await
}

/// Run the `agrafe render-text` helper on `sample`, drawing at twice the
/// box so the downscale leaves crisp glyphs, writing a page to `output`
/// when the sample is text.
async fn render_text(
    media: &MediaConfig,
    sample: &Path,
    output: &Path,
    fit: Fit,
) -> Result<Helper, JobError> {
    let mut command = helper(media)?;
    command
        .arg("render-text")
        .arg("--input")
        .arg(sample)
        .arg("--output")
        .arg(output)
        .arg("--width")
        .arg((fit.width * 2).to_string())
        .arg("--height")
        .arg((fit.height * 2).to_string());
    run_helper(media, command, output, "text").await
}

/// This binary, re-executed in the sandbox.
fn helper(media: &MediaConfig) -> Result<tokio::process::Command, JobError> {
    let program = match &media.agrafe_command {
        Some(path) => path.clone(),
        None => std::env::current_exe()?,
    };
    Ok(sandbox::command(program))
}

/// A helper that succeeds without writing `output` found nothing to do.
async fn run_helper(
    media: &MediaConfig,
    command: tokio::process::Command,
    output: &Path,
    what: &str,
) -> Result<Helper, JobError> {
    match sandbox::run(command, &media.sandbox).await? {
        Outcome::Success => Ok(if tokio::fs::try_exists(output).await? {
            Helper::Produced
        } else {
            Helper::Declined
        }),
        Outcome::Failed { status, stderr } => {
            tracing::warn!(status, stderr, "the {what} helper failed");
            Ok(Helper::Failed)
        }
        Outcome::TimedOut => {
            tracing::warn!("the {what} helper timed out");
            Ok(Helper::Failed)
        }
    }
}

/// Why a worker may not start: its tools do not work on files known to be
/// good.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("rendering {what} with `{vips}`: {source}")]
    Render {
        what: &'static str,
        vips: String,
        source: render::Error,
    },
    #[error("the render-text helper {what}; is `media.agrafe_command` this binary?")]
    Helper { what: &'static str },
    #[error(transparent)]
    Run(#[from] JobError),
    #[error("temporary files: {0}")]
    Io(#[from] std::io::Error),
}

const PROBE_PNG: &[u8] = include_bytes!("../../assets/probe/pixel.png");
const PROBE_PDF: &[u8] = include_bytes!("../../assets/probe/page.pdf");

/// Render a text sample, a one-pixel PNG and a one-page PDF through the
/// exact command lines the jobs use. Run before a worker starts: vips exits
/// non-zero for a bad file and for a missing loader or an unknown option
/// alike, so without this a libvips built without PDF support, or too old
/// for an option, would mark every file `failed` instead of failing the
/// deployment.
pub async fn probe(media: &MediaConfig) -> Result<(), ProbeError> {
    let workdir = workdir(media)?;
    let fit = Fit::thumbnail(media);
    let sample = workdir.path().join("sample.txt");
    tokio::fs::write(&sample, b"agrafe\n").await?;
    let page = workdir.path().join("page.pgm");
    match render_text(media, &sample, &page, fit).await? {
        Helper::Produced => {}
        Helper::Declined => {
            return Err(ProbeError::Helper {
                what: "declined a text sample",
            });
        }
        Helper::Failed => {
            return Err(ProbeError::Helper {
                what: "failed on a text sample",
            });
        }
    }
    let pixel = workdir.path().join("pixel.png");
    tokio::fs::write(&pixel, PROBE_PNG).await?;
    let pdf = workdir.path().join("page.pdf");
    tokio::fs::write(&pdf, PROBE_PDF).await?;
    let output = workdir.path().join("probe.webp");
    for (what, input, kind) in [
        ("a PNG", &pixel, Input::Raster),
        ("a PDF page", &pdf, Input::Page),
        ("a text page", &page, Input::Raster),
    ] {
        render::to_webp(media, input, kind, &output, fit)
            .await
            .map_err(|source| ProbeError::Render {
                what,
                vips: media.vips_command.clone(),
                source,
            })?;
    }
    Ok(())
}
