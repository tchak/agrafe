//! First-page thumbnails for office documents through an optional
//! LibreOffice. LibreOffice does one thing here: convert the file to a PDF
//! in the job's work directory, from which the first page then takes the
//! path a PDF thumbnail takes, `vips thumbnail` with the `Page` input.
//! LibreOffice never produces the picture, so there is one raster path
//! and a DOCX and a PDF look the same.
//!
//! One process per document, through `agrafe_sandbox` like every other
//! tool, with a private user profile under the work directory: nothing a
//! document does persists past the job, and concurrent workers never
//! share a profile. Off by default: a deployment without `command` gets
//! the icon for office files, as before.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agrafe_sandbox as sandbox;
use agrafe_types::classify::{DOCX, ODS, ODT, XLSX};
use agrafe_types::sniff;
use bytesize::ByteSize;
use serde::Deserialize;

use crate::{Config, Error, Rejection, Verdict};

/// The `[media.office]` table: the converter and its own limits.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Office {
    /// The LibreOffice binary, a bare name looked up on `PATH` or a path.
    /// Unset by default, and office files get the icon.
    #[serde(default)]
    pub command: Option<String>,
    /// A user profile warmed at image build time, copied into the work
    /// directory for every run. Unset, LibreOffice creates one per run.
    #[serde(default)]
    pub profile_template: Option<PathBuf>,
    /// The `[media.office.sandbox]` table: the limits that differ from
    /// `[media.sandbox]`.
    #[serde(default)]
    pub sandbox: OfficeSandbox,
}

/// LibreOffice needs more address space and more time than vips; the rest
/// of its sandbox (the output size, the isolation, the extra readable
/// paths) is `[media.sandbox]`'s.
#[derive(Debug, Clone, Deserialize)]
pub struct OfficeSandbox {
    /// Wall-clock limit per conversion, two minutes by default.
    #[serde(with = "humantime_serde", default = "default_timeout")]
    pub timeout: Duration,
    /// Address space limit, 2 GiB by default.
    #[serde(default = "default_memory")]
    pub memory: ByteSize,
}

fn default_timeout() -> Duration {
    Duration::from_secs(120)
}
fn default_memory() -> ByteSize {
    ByteSize::gib(2)
}

impl Default for OfficeSandbox {
    fn default() -> Self {
        Self {
            timeout: default_timeout(),
            memory: default_memory(),
        }
    }
}

impl Office {
    /// The sandbox a conversion runs under: `base`, the `[media.sandbox]`
    /// table, with this table's limits.
    pub fn sandbox(&self, base: &sandbox::Config) -> sandbox::Config {
        sandbox::Config {
            timeout: self.sandbox.timeout,
            memory: self.sandbox.memory,
            ..base.clone()
        }
    }
}

/// How LibreOffice reads a content type and writes it out: the extension
/// it picks its import filter from, and the PDF export filter of the
/// application that owns the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Document {
    /// The extension the input is given in the work directory.
    pub extension: &'static str,
    /// `writer_pdf_Export` or `calc_pdf_Export`.
    pub filter: &'static str,
}

const WRITER: &str = "writer_pdf_Export";
const CALC: &str = "calc_pdf_Export";

/// What LibreOffice converts, by content type: DOCX and ODT through
/// Writer, XLSX and ODS through Calc. `None` for anything else.
pub fn document(content_type: &str) -> Option<Document> {
    let (extension, filter) = match sniff::canonical(content_type) {
        DOCX => ("docx", WRITER),
        ODT => ("odt", WRITER),
        XLSX => ("xlsx", CALC),
        ODS => ("ods", CALC),
        _ => return None,
    };
    Some(Document { extension, filter })
}

/// The directories LibreOffice's launcher binds the single-instance pipe
/// in, a unix socket it cannot start without: `/tmp` when the user may
/// write there, else `/var/tmp`, and it refuses to start otherwise. The
/// sandbox lets it make and remove socket files in the one it will pick,
/// and nothing else there.
const PIPE_DIRS: [&str; 2] = ["/tmp", "/var/tmp"];

/// The directory LibreOffice will put its pipe in, mirroring its choice:
/// the first of [`PIPE_DIRS`] this process, which is the same user, can
/// write to.
pub(crate) fn pipe_dir() -> &'static str {
    PIPE_DIRS
        .into_iter()
        .find(|dir| tempfile::tempfile_in(dir).is_ok())
        .unwrap_or(PIPE_DIRS[0])
}

/// Debian's layout keeps LibreOffice's registry and printer configuration
/// under `/etc`, which the sandbox's system set does not cover; skipped
/// where it does not exist, and `media.sandbox.readable` is for other
/// layouts.
const ETC: &str = "/etc/libreoffice";

/// The file the input is linked to in the conversion directory, with the
/// document's extension, and the stem LibreOffice names the PDF after.
const INPUT: &str = "document";

/// Convert `original`, a file of `content_type`, to a one-page PDF in the
/// `office` directory under `workdir`, and return the PDF's path.
/// `Declined` when no converter is configured or the type is not one
/// LibreOffice converts, and when LibreOffice exited cleanly without
/// writing a PDF, which is how it reports a file it cannot load;
/// `Rejected` when it failed or ran out of time.
pub async fn convert(
    config: &Config,
    content_type: &str,
    original: &Path,
    workdir: &Path,
) -> Result<Verdict<PathBuf>, Error> {
    let (Some(command), Some(document)) = (&config.office.command, document(content_type)) else {
        return Ok(Verdict::Declined);
    };
    let dir = workdir.join("office");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(Error::Input)?;
    let input = dir.join(format!("{INPUT}.{}", document.extension));
    // LibreOffice picks its import filter from the name: the original is
    // linked under the one its content type implies, or copied where the
    // filesystem will not link.
    if tokio::fs::hard_link(original, &input).await.is_err() {
        tokio::fs::copy(original, &input)
            .await
            .map_err(Error::Input)?;
    }
    let profile = dir.join("profile");
    if let Some(template) = &config.office.profile_template {
        copy_dir(template.clone(), profile.clone())
            .await
            .map_err(Error::Input)?;
    }
    let output = dir.join(format!("{INPUT}.pdf"));

    let mut command = sandbox::Command::new(command);
    command
        .current_dir(&dir)
        .writable(&dir)
        .readable(ETC)
        .unix_sockets(pipe_dir())
        // Where LibreOffice stages the PDF before moving it to `--outdir`,
        // and where its caches land, all gone with the job.
        .env("TMPDIR", &dir)
        .env("HOME", &dir)
        .env("XDG_CACHE_HOME", &dir)
        .arg(format!(
            "-env:UserInstallation=file://{}",
            profile.display()
        ))
        .args(["--headless", "--norestore", "--nologo", "--nolockcheck"])
        .arg("--convert-to")
        // The JSON filter options (LibreOffice 7.4 and later) lay out and
        // write the first page only; the probe checks they are honoured.
        .arg(format!(
            "pdf:{}:{{\"PageRange\":{{\"type\":\"string\",\"value\":\"1\"}}}}",
            document.filter
        ))
        .arg("--outdir")
        .arg(&dir)
        .arg(&input);
    let outcome = sandbox::run(command, &config.office.sandbox(&config.sandbox)).await?;
    if let Some(rejection) = Rejection::of(&outcome) {
        return Ok(Verdict::Rejected(rejection));
    }
    Ok(
        if tokio::fs::try_exists(&output)
            .await
            .map_err(Error::Output)?
        {
            Verdict::Rendered(output)
        } else {
            Verdict::Declined
        },
    )
}

/// Copy the directory `from` to `to`, which must not exist, following no
/// symlink: a profile template is a few hundred small files.
async fn copy_dir(from: PathBuf, to: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || copy_dir_blocking(&from, &to))
        .await
        .map_err(std::io::Error::other)?
}

fn copy_dir_blocking(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir_blocking(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_by_type() {
        assert_eq!(document(DOCX).unwrap().extension, "docx");
        assert_eq!(document(DOCX).unwrap().filter, WRITER);
        assert_eq!(document(ODT).unwrap().filter, WRITER);
        assert_eq!(document(XLSX).unwrap().extension, "xlsx");
        assert_eq!(document(XLSX).unwrap().filter, CALC);
        assert_eq!(document(ODS).unwrap().filter, CALC);
        assert_eq!(document("application/pdf"), None);
        assert_eq!(document("text/csv"), None);
    }

    #[test]
    fn office_sandbox_takes_its_limits_and_the_rest_from_the_base() {
        let office = Office {
            command: Some("soffice".into()),
            ..Office::default()
        };
        let base = sandbox::Config {
            max_output_size: ByteSize::mib(10),
            isolation: sandbox::Isolation::Required,
            readable: vec!["/opt/fonts".into()],
            ..sandbox::Config::default()
        };
        let config = office.sandbox(&base);
        assert_eq!(config.timeout, Duration::from_secs(120));
        assert_eq!(config.memory, ByteSize::gib(2));
        assert_eq!(config.max_output_size, ByteSize::mib(10));
        assert_eq!(config.isolation, sandbox::Isolation::Required);
        assert_eq!(config.readable, vec![PathBuf::from("/opt/fonts")]);
    }

    #[test]
    fn parses_the_table() {
        let office: Office = toml::from_str(
            r#"
            command = "/opt/libreoffice/program/soffice"
            profile_template = "/var/lib/agrafe/soffice-profile"
            [sandbox]
            timeout = "30s"
            "#,
        )
        .unwrap();
        assert_eq!(
            office.command.as_deref(),
            Some("/opt/libreoffice/program/soffice")
        );
        assert_eq!(
            office.profile_template.as_deref(),
            Some(Path::new("/var/lib/agrafe/soffice-profile"))
        );
        assert_eq!(office.sandbox.timeout, Duration::from_secs(30));
        assert_eq!(office.sandbox.memory, ByteSize::gib(2));
        let office: Office = toml::from_str("").unwrap();
        assert_eq!(office.command, None);
    }

    #[tokio::test]
    async fn declines_without_a_command_or_for_other_types() {
        let workdir = tempfile::tempdir().unwrap();
        let original = workdir.path().join("original");
        std::fs::write(&original, b"PK").unwrap();
        let config = Config::default();
        let verdict = convert(&config, DOCX, &original, workdir.path())
            .await
            .unwrap();
        assert_eq!(verdict, Verdict::Declined);
        let config = Config {
            office: Office {
                command: Some("/nonexistent/soffice".into()),
                ..Office::default()
            },
            ..Config::default()
        };
        let verdict = convert(&config, "application/pdf", &original, workdir.path())
            .await
            .unwrap();
        assert_eq!(verdict, Verdict::Declined);
    }

    #[tokio::test]
    async fn copies_a_profile_template() {
        let root = tempfile::tempdir().unwrap();
        let template = root.path().join("template");
        std::fs::create_dir_all(template.join("user/config")).unwrap();
        std::fs::write(template.join("user/registrymodifications.xcu"), b"<xcu/>").unwrap();
        std::fs::write(template.join("user/config/a.xml"), b"<a/>").unwrap();
        let copy = root.path().join("profile");
        copy_dir(template, copy.clone()).await.unwrap();
        assert_eq!(
            std::fs::read(copy.join("user/registrymodifications.xcu")).unwrap(),
            b"<xcu/>"
        );
        assert_eq!(
            std::fs::read(copy.join("user/config/a.xml")).unwrap(),
            b"<a/>"
        );
    }
}
