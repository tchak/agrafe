//! A check of the tools on known-good files, run before a worker starts.

use std::path::Path;

use agrafe_types::office::{DOCX, ODG, ODT, PPTX, XLSX};

use crate::vips::{self, Fit, Input};
use agrafe_types::map::Format;

use crate::{Config, Error, Rejection, Verdict, geo, helpers, office};

/// Why a worker may not start: its tools do not work on files known to be
/// good.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// `vips` could not be run on a probe file, or its output not read.
    #[error("rendering {what} with `{vips}`: {source}")]
    Render {
        /// The probe file, as `"a PNG"` or `"a map"`.
        what: &'static str,
        /// The configured `vips_command`.
        vips: String,
        /// Why it could not run.
        source: Error,
    },
    /// `vips` ran and rejected a probe file: a loader or an option this
    /// build of libvips lacks.
    #[error("rendering {what} with `{vips}`: {rejection}")]
    Rejected {
        /// The probe file, as `"a PNG"` or `"a map"`.
        what: &'static str,
        /// The configured `vips_command`.
        vips: String,
        /// How it failed.
        rejection: Rejection,
    },
    /// A helper declined or rejected its probe file: the command
    /// configured is likely not that helper.
    #[error("`{command}` {what}; is media.{setting} the {helper} helper?")]
    Helper {
        /// The command configured.
        command: String,
        /// The `[media]` key that names it.
        setting: &'static str,
        /// The helper it should be.
        helper: &'static str,
        /// What it did instead of rendering.
        what: String,
    },
    /// The configured LibreOffice declined or rejected a probe document,
    /// wrote more than the first page, or ran a document's macro: it is
    /// not LibreOffice, lacks Writer, Calc, Impress or Draw, or is too
    /// old for the JSON filter options.
    #[error(
        "`{command}` {what}; is media.office.command a LibreOffice 7.4 or later \
         with Writer, Calc, Impress and Draw?"
    )]
    Office {
        /// The configured `office.command`.
        command: String,
        /// What it did instead of converting.
        what: String,
    },
    /// `media.office.command` is set, but the sandbox has neither Landlock
    /// nor bubblewrap and isolation is not `off`: nothing would keep a
    /// document from reading what the worker can.
    #[error(
        "media.office.command is set but the sandbox is {status}: without Landlock or \
         bubblewrap, LibreOffice reads every file the worker can, the configuration and \
         the keys included, and a document can lay one out on its first page; set \
         media.sandbox.isolation = \"off\" to accept that on a development machine"
    )]
    OfficeUnconfined {
        /// The sandbox status, as the boot line prints it.
        status: String,
    },
    /// `media.office.command` is set and the work directories are under
    /// the directory LibreOffice may remove files in.
    #[error(
        "media.office.command is set and the work directories are under {tmp_dir}, \
         where the sandbox lets LibreOffice remove files for its pipe: set \
         media.tmp_dir to a directory outside {pipe_dir}"
    )]
    OfficeTmpDir {
        /// The effective `media.tmp_dir`.
        tmp_dir: String,
        /// Where LibreOffice puts its pipe.
        pipe_dir: &'static str,
    },
    /// A helper could not be run at all.
    #[error(transparent)]
    Run(#[from] Error),
    /// The sandbox does not confine as it should.
    #[error("sandbox: {0}")]
    Sandbox(#[from] agrafe_sandbox::ProbeError),
    /// The probe's work directory or files could not be written.
    #[error("temporary files: {0}")]
    Io(#[from] std::io::Error),
}

const PROBE_PNG: &[u8] = include_bytes!("../assets/probe/pixel.png");
const PROBE_PDF: &[u8] = include_bytes!("../assets/probe/page.pdf");
const PROBE_IMAGE_PDF: &[u8] = include_bytes!("../assets/probe/image.pdf");
const PROBE_GEOJSON: &[u8] = include_bytes!("../assets/probe/point.geojson");
/// Two pages of text: the exported PDF must have one.
const PROBE_DOCX: &[u8] = include_bytes!("../assets/probe/pages.docx");
/// One sheet of a few cells.
const PROBE_XLSX: &[u8] = include_bytes!("../assets/probe/sheet.xlsx");
/// Two slides: the exported PDF must have one.
const PROBE_PPTX: &[u8] = include_bytes!("../assets/probe/slides.pptx");
/// Two drawing pages: the exported PDF must have one.
const PROBE_ODG: &[u8] = include_bytes!("../assets/probe/drawing.odg");
/// One page, and a Basic macro bound to the document's load event that
/// creates a folder next to the document: the folder must not exist once
/// the conversion is done.
const PROBE_MACRO: &[u8] = include_bytes!("../assets/probe/macro.odt");

/// The probe's typst workspace: a template importing a file, reading
/// data and placing an image, as templates do.
const PROBE_DOCUMENT: &[(&str, &[u8])] = &[
    (
        "main.typ",
        include_bytes!("../assets/probe/document/main.typ"),
    ),
    (
        "lib/header.typ",
        include_bytes!("../assets/probe/document/lib/header.typ"),
    ),
    (
        "data.json",
        include_bytes!("../assets/probe/document/data.json"),
    ),
    ("pixel.png", PROBE_PNG),
];

/// Check that the sandbox confines what it should, before any tool runs
/// in it: what the worker gets, for its boot log, or why it may not start.
/// With `media.sandbox.isolation = "required"` a kernel that cannot
/// confine is a startup error.
pub async fn probe_sandbox(
    config: &agrafe_sandbox::Config,
) -> Result<agrafe_sandbox::Status, ProbeError> {
    Ok(agrafe_sandbox::probe(config).await?)
}

/// Check that the host confines the office converter, before it runs
/// anything: with `media.office.command` set, the sandbox must confine
/// the filesystem, with Landlock or bubblewrap's mount namespace, and the
/// work directories must be outside the directory LibreOffice puts its
/// pipe in. A document names files to LibreOffice (linked sections, DDE,
/// linked images), and the filesystem confinement is what keeps that to
/// the work directory: without it the configuration and the keys are a
/// 72-point paragraph away from the thumbnail. And since Landlock has no
/// "remove sockets only" right, the pipe directory is one LibreOffice may
/// remove files in, so no other job's files may be there (under
/// bubblewrap the pipe lands in a private `/tmp`, and the rule costs
/// nothing to keep). Isolation `off` is the operator accepting both, on a
/// development machine.
pub fn check_office_host(
    config: &Config,
    tmp_dir: Option<&Path>,
    sandbox: &agrafe_sandbox::Status,
) -> Result<(), ProbeError> {
    if config.office.command.is_none() || config.sandbox.isolation == agrafe_sandbox::Isolation::Off
    {
        return Ok(());
    }
    if !sandbox.confines_filesystem() {
        return Err(ProbeError::OfficeUnconfined {
            status: sandbox.to_string(),
        });
    }
    let pipe_dir = office::pipe_dir();
    let work = tmp_dir.map_or_else(std::env::temp_dir, Path::to_path_buf);
    if canonical(&work).starts_with(canonical(Path::new(pipe_dir))) {
        return Err(ProbeError::OfficeTmpDir {
            tmp_dir: work.display().to_string(),
            pipe_dir,
        });
    }
    Ok(())
}

/// `path` with its symlinks resolved, so that `/tmp` and `/private/tmp`
/// compare equal; the path as given when it cannot be.
fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Render a text sample, a one-pixel PNG, a one-page PDF, the JPEG pulled
/// out of a PDF that wraps one and the map of a one-point GeoJSON through
/// the exact command lines the jobs use. vips exits non-zero for a bad file
/// and for a missing loader or an unknown option alike, so without this a
/// libvips built without PDF or SVG support, or too old for an option,
/// would mark every file `failed` instead of failing the deployment. The
/// text sample, the wrapped JPEG and the map also catch a
/// `render_text_command`, an `extract_command` or a `render_geo_command`
/// that is not the helper: a missing one fails every job, a wrong one
/// every preview of its kind. The map is drawn with no background layer: the probe never
/// touches the network. With `media.office.command` set, a document for
/// each LibreOffice application goes through it as well, see
/// [`probe_office`].
pub async fn probe(config: &Config, tmp_dir: Option<&Path>) -> Result<(), ProbeError> {
    probe_tools(config, tmp_dir).await?;
    if config.office.command.is_some() {
        probe_office(config, tmp_dir).await?;
    }
    Ok(())
}

async fn probe_tools(config: &Config, tmp_dir: Option<&Path>) -> Result<(), ProbeError> {
    let workdir = crate::workdir(tmp_dir)?;
    let fit = Fit::thumbnail(config);
    let sample = workdir.path().join("sample.txt");
    tokio::fs::write(&sample, b"agrafe\n").await?;
    let page = workdir.path().join("page.pgm");
    let what = match helpers::render_text(config, &sample, &page, fit).await? {
        Verdict::Rendered(()) => None,
        Verdict::Declined => Some("declined a text sample".to_string()),
        Verdict::Rejected(rejection) => Some(format!("failed on a text sample: {rejection}")),
    };
    if let Some(what) = what {
        return Err(ProbeError::Helper {
            command: config.render_text_command.clone(),
            setting: "render_text_command",
            helper: "agrafe-render-text",
            what,
        });
    }
    let wrapped = workdir.path().join("image.pdf");
    tokio::fs::write(&wrapped, PROBE_IMAGE_PDF).await?;
    let embedded = workdir.path().join("embedded");
    let what = match helpers::extract(config, "application/pdf", &wrapped, &embedded).await? {
        Verdict::Rendered(()) => None,
        Verdict::Declined => Some("declined a PDF wrapping a JPEG".to_string()),
        Verdict::Rejected(rejection) => {
            Some(format!("failed on a PDF wrapping a JPEG: {rejection}"))
        }
    };
    if let Some(what) = what {
        return Err(ProbeError::Helper {
            command: config.extract_command.clone(),
            setting: "extract_command",
            helper: "agrafe-extract",
            what,
        });
    }
    let pixel = workdir.path().join("pixel.png");
    tokio::fs::write(&pixel, PROBE_PNG).await?;
    let pdf = workdir.path().join("page.pdf");
    tokio::fs::write(&pdf, PROBE_PDF).await?;
    let output = workdir.path().join("probe.webp");
    for (what, input, kind) in [
        ("a PNG", &pixel, Input::Raster),
        ("a PDF page", &pdf, Input::Page),
        ("a JPEG", &embedded, Input::Raster),
        ("a text page", &page, Input::Raster),
    ] {
        let vips = config.vips_command.clone();
        let verdict = vips::to_webp(config, input, kind, &output, fit)
            .await
            .map_err(|source| ProbeError::Render {
                what,
                vips: vips.clone(),
                source,
            })?;
        if let Verdict::Rejected(rejection) = verdict {
            return Err(ProbeError::Rejected {
                what,
                vips,
                rejection,
            });
        }
    }
    let geojson = workdir.path().join("point.geojson");
    tokio::fs::write(&geojson, PROBE_GEOJSON).await?;
    let spec = workdir.path().join(geo::SPEC);
    let bare = Config {
        map: crate::Map {
            layers: Vec::new(),
            ..config.map.clone()
        },
        ..config.clone()
    };
    tokio::fs::write(
        &spec,
        serde_json::to_vec(&bare.map.spec(Format::GeoJson, fit)).expect("a spec serialises"),
    )
    .await?;
    let svg = workdir.path().join(geo::PATHS);
    let extent = workdir.path().join(geo::EXTENT);
    let what = match helpers::render_geo(config, &geojson, &spec, &svg, &extent).await? {
        Verdict::Rendered(()) => None,
        Verdict::Declined => Some("declined a one-point GeoJSON".to_string()),
        Verdict::Rejected(rejection) => Some(format!("failed on a one-point GeoJSON: {rejection}")),
    };
    if let Some(what) = what {
        return Err(ProbeError::Helper {
            command: config.render_geo_command.clone(),
            setting: "render_geo_command",
            helper: "agrafe-render-geo",
            what,
        });
    }
    let vips = config.vips_command.clone();
    match vips::composite(config, workdir.path(), &[], geo::PATHS, geo::MAP)
        .await
        .map_err(|source| ProbeError::Render {
            what: "a map",
            vips: vips.clone(),
            source,
        })? {
        Verdict::Rendered(()) | Verdict::Declined => {}
        Verdict::Rejected(rejection) => {
            return Err(ProbeError::Rejected {
                what: "a map",
                vips,
                rejection,
            });
        }
    }
    let map = workdir.path().join(geo::MAP);
    let verdict = vips::to_webp(config, &map, Input::Raster, &output, fit)
        .await
        .map_err(|source| ProbeError::Render {
            what: "a map",
            vips: vips.clone(),
            source,
        })?;
    if let Verdict::Rejected(rejection) = verdict {
        return Err(ProbeError::Rejected {
            what: "a map",
            vips,
            rejection,
        });
    }
    Ok(())
}

/// Convert a two-page DOCX, a two-page XLSX, a two-slide PPTX, a
/// two-page ODG and an ODT carrying a macro through the exact command
/// lines the render job uses, and render the results through vips: a
/// LibreOffice without one of its four applications (Debian packages them
/// apart), without fonts, or too old for the JSON filter options would
/// mark every office file of that kind `failed` instead of failing the
/// deployment. The DOCX, the XLSX, the PPTX and the ODG have two pages so that
/// `PageRange` is known to be honoured by each export filter: the PDF
/// must have one, which vips tells by refusing the second. The ODT binds
/// a Basic macro to its load event that creates a folder next to the
/// document; the folder must not be there afterwards, which is the
/// conversion running no macro (see `office::pin_macro_security`).
pub async fn probe_office(config: &Config, tmp_dir: Option<&Path>) -> Result<(), ProbeError> {
    let command = config.office.command.clone().unwrap_or_default();
    let workdir = crate::workdir(tmp_dir)?;
    let fit = Fit::thumbnail(config);
    let output = workdir.path().join("probe.webp");
    for (what, name, content_type, bytes, two_pages) in [
        ("a two-page DOCX", "writer", DOCX, PROBE_DOCX, true),
        ("a two-page XLSX", "calc", XLSX, PROBE_XLSX, true),
        ("a two-slide PPTX", "impress", PPTX, PROBE_PPTX, true),
        ("a two-page ODG", "draw", ODG, PROBE_ODG, true),
        ("an ODT with a macro", "macro", ODT, PROBE_MACRO, false),
    ] {
        let dir = workdir.path().join(name);
        tokio::fs::create_dir(&dir).await?;
        let original = dir.join("original");
        tokio::fs::write(&original, bytes).await?;
        let pdf = match office::convert(config, content_type, &original, &dir).await? {
            Verdict::Rendered(pdf) => pdf,
            Verdict::Declined => {
                return Err(ProbeError::Office {
                    command,
                    what: format!("wrote no PDF for {what}"),
                });
            }
            Verdict::Rejected(rejection) => {
                return Err(ProbeError::Office {
                    command,
                    what: format!("failed on {what}: {rejection}"),
                });
            }
        };
        let vips = config.vips_command.clone();
        let render = |page| vips::page_to_webp(config, &pdf, page, &output, fit);
        if let Verdict::Rejected(rejection) =
            render(0).await.map_err(|source| ProbeError::Render {
                what,
                vips: vips.clone(),
                source,
            })?
        {
            return Err(ProbeError::Rejected {
                what,
                vips,
                rejection,
            });
        }
        if two_pages
            && let Verdict::Rendered(_) = render(1).await.map_err(|source| ProbeError::Render {
                what,
                vips: vips.clone(),
                source,
            })?
        {
            return Err(ProbeError::Office {
                command,
                what: format!(
                    "wrote more than the first page of {what}, so its export filter ignores \
                     the PageRange option"
                ),
            });
        }
        // The macro creates `<document URL>.macro-ran` next to the input,
        // which `convert` linked in as `document.odt` beside the PDF.
        if name == "macro" && tokio::fs::try_exists(pdf.with_extension("odt.macro-ran")).await? {
            return Err(ProbeError::Office {
                command,
                what: "ran the macro bound to the load event of an ODT, which neither the \
                       conversion nor the pinned macro security stopped"
                    .to_string(),
            });
        }
    }
    Ok(())
}

/// Compile the probe workspace through the exact command line the
/// `document` build uses, before a worker takes documents: templates are
/// written against a compiler version, and a `render_document_command`
/// that is not the helper, or a helper that cannot read its workspace,
/// would kill every document job instead of failing the deployment.
pub async fn probe_document(config: &Config, tmp_dir: Option<&Path>) -> Result<(), ProbeError> {
    let workdir = crate::workdir(tmp_dir)?;
    let workspace = workdir.path().join("workspace");
    for (path, bytes) in PROBE_DOCUMENT {
        let path = workspace.join(path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, bytes).await?;
    }
    let output = workdir.path().join("probe.pdf");
    let what = match helpers::render_document(config, &workspace, "main.typ", &output).await? {
        Verdict::Rendered(()) => None,
        Verdict::Declined => Some("wrote nothing for the probe template".to_string()),
        Verdict::Rejected(rejection) => Some(format!("failed on the probe template: {rejection}")),
    };
    if let Some(what) = what {
        return Err(ProbeError::Helper {
            command: config.render_document_command.clone(),
            setting: "render_document_command",
            helper: "agrafe-render-document",
            what,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Office;

    fn with_office(isolation: agrafe_sandbox::Isolation) -> Config {
        Config {
            office: Office {
                command: Some("soffice".into()),
                ..Office::default()
            },
            sandbox: agrafe_sandbox::Config {
                isolation,
                ..agrafe_sandbox::Config::default()
            },
            ..Config::default()
        }
    }

    fn status(landlock: Option<i32>) -> agrafe_sandbox::Status {
        agrafe_sandbox::Status {
            landlock,
            seccomp: true,
            bubblewrap: None,
            rlimits_only: None,
        }
    }

    #[test]
    fn the_office_converter_needs_landlock_or_bubblewrap() {
        // Outside the pipe directory, which the system temporary directory
        // is on Linux: under the crate, removed on drop.
        let elsewhere = tempfile::Builder::new()
            .prefix(".agrafe-probe-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let config = with_office(agrafe_sandbox::Isolation::Auto);
        let err = check_office_host(&config, Some(elsewhere.path()), &status(None)).unwrap_err();
        assert!(matches!(err, ProbeError::OfficeUnconfined { .. }), "{err}");
        assert!(err.to_string().contains("no landlock"), "{err}");
        check_office_host(&config, Some(elsewhere.path()), &status(Some(4))).unwrap();
        // bubblewrap's mount namespace stands in for Landlock.
        let wrapped = agrafe_sandbox::Status {
            bubblewrap: Some("0.11.1".to_string()),
            ..status(None)
        };
        check_office_host(&config, Some(elsewhere.path()), &wrapped).unwrap();

        // No converter, or isolation off: nothing to check.
        check_office_host(&Config::default(), None, &status(None)).unwrap();
        let off = with_office(agrafe_sandbox::Isolation::Off);
        check_office_host(&off, None, &status(None)).unwrap();
    }

    #[test]
    fn the_work_directories_stay_out_of_the_pipe_directory() {
        let config = with_office(agrafe_sandbox::Isolation::Auto);
        let inside = tempfile::tempdir_in(office::pipe_dir()).unwrap();
        let err = check_office_host(&config, Some(inside.path()), &status(Some(4))).unwrap_err();
        assert!(matches!(err, ProbeError::OfficeTmpDir { .. }), "{err}");
        assert!(err.to_string().contains("media.tmp_dir"), "{err}");
        // The pipe directory itself, as the default is on Linux.
        let err = check_office_host(
            &config,
            Some(Path::new(office::pipe_dir())),
            &status(Some(4)),
        )
        .unwrap_err();
        assert!(matches!(err, ProbeError::OfficeTmpDir { .. }), "{err}");
    }
}
