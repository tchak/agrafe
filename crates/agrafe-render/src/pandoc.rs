//! pandoc, run in the sandbox: the one tool for conversions between
//! document formats. A conversion names pandoc's reader and writer, never
//! guessed from a file name, and runs with pandoc's own `--sandbox` by
//! default, which turns off its access to files and the network beyond
//! its input: no includes, no remote or local images fetched, no filter
//! I/O. The `document` build uses it to turn the HTML a template compiled
//! to into a DOCX or an ODT, and reads the result back in the probe.
//!
//! Writing a DOCX or an ODT is the exception: pandoc assembles them from
//! data files, which a distribution's build reads from
//! `/usr/share/pandoc` rather than embeds, and its `--sandbox` refuses
//! those reads too ("Could not find data file data/data/docx/…"). The
//! input there is HTML the helper wrote with no reference outside it,
//! and `agrafe_sandbox` still confines pandoc: on Linux it reads the
//! system set and its declared files, and opens no socket.

use std::path::Path;

use agrafe_sandbox as sandbox;
use agrafe_types::office::{DOCX, ODT};

use crate::{Config, Error, Rejection, Verdict};

/// The exit code of a pandoc whose heap ran out, GHC's: the sandbox's
/// memory limit, which a later attempt on a less busy host may not meet,
/// unlike pandoc's other failures, which are about the input.
pub const OUT_OF_MEMORY_EXIT: i32 = 251;

/// One run of pandoc: `input` read as `from`, written as `to` to `output`,
/// in `output`'s directory, the one it may write.
#[derive(Debug, Clone)]
pub struct Conversion<'a> {
    from: &'a str,
    to: &'a str,
    input: &'a Path,
    output: &'a Path,
    reference: Option<&'a Path>,
    args: Vec<&'a str>,
    own_sandbox: bool,
}

impl<'a> Conversion<'a> {
    /// `input` read with pandoc's `from` reader and written with its `to`
    /// writer to `output`.
    pub fn new(from: &'a str, to: &'a str, input: &'a Path, output: &'a Path) -> Self {
        Self {
            from,
            to,
            input,
            output,
            reference: None,
            args: Vec::new(),
            own_sandbox: true,
        }
    }

    /// The document whose styles a DOCX or an ODT takes,
    /// `--reference-doc`.
    pub fn reference_doc(mut self, path: &'a Path) -> Self {
        self.reference = Some(path);
        self
    }

    /// Without pandoc's `--sandbox`, for a writer that needs pandoc's data
    /// files from disk; only for an input that refers to nothing outside
    /// it.
    pub fn without_own_sandbox(mut self) -> Self {
        self.own_sandbox = false;
        self
    }

    /// An option of pandoc's own, such as `--wrap=none`.
    pub fn arg(mut self, arg: &'a str) -> Self {
        self.args.push(arg);
        self
    }
}

/// Run `conversion` under `media.sandbox`. `Rendered` when pandoc wrote
/// the output, `Rejected` when it failed, was killed or ran out of time,
/// `Declined` when it exited cleanly without writing.
pub async fn convert(config: &Config, conversion: &Conversion<'_>) -> Result<Verdict<()>, Error> {
    let dir = conversion.output.parent().unwrap_or(Path::new("."));
    let mut command = sandbox::Command::new(&config.pandoc_command);
    command
        .current_dir(dir)
        .writable(dir)
        .readable(conversion.input)
        // pandoc looks for its user data directory under `HOME`.
        .env("HOME", dir)
        .env("XDG_CACHE_HOME", dir);
    if conversion.own_sandbox {
        command.arg("--sandbox");
    }
    command
        .arg("--from")
        .arg(conversion.from)
        .arg("--to")
        .arg(conversion.to)
        .args(&conversion.args);
    if let Some(reference) = conversion.reference {
        command
            .readable(reference)
            .arg(format!("--reference-doc={}", reference.display()));
    }
    command
        .arg("--output")
        .arg(conversion.output)
        // After `--`, so that no file name is read as an option.
        .arg("--")
        .arg(conversion.input);
    let outcome = sandbox::run(command, &config.sandbox).await?;
    if let Some(rejection) = Rejection::of(&outcome) {
        return Ok(Verdict::Rejected(rejection));
    }
    Ok(
        if tokio::fs::try_exists(conversion.output)
            .await
            .map_err(Error::Output)?
        {
            Verdict::Rendered(())
        } else {
            Verdict::Declined
        },
    )
}

/// The editable formats a `document` blob builds to: typst compiles the
/// workspace to HTML, and pandoc writes the HTML as one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeFormat {
    /// Office Open XML, Word's.
    Docx,
    /// OpenDocument Text.
    Odt,
}

impl OfficeFormat {
    /// The format a declared content type asks for, `None` for any other
    /// type than DOCX and ODT.
    pub fn of(content_type: &str) -> Option<Self> {
        match content_type {
            DOCX => Some(OfficeFormat::Docx),
            ODT => Some(OfficeFormat::Odt),
            _ => None,
        }
    }

    /// pandoc's reader and writer for the format.
    pub fn name(self) -> &'static str {
        match self {
            OfficeFormat::Docx => "docx",
            OfficeFormat::Odt => "odt",
        }
    }

    /// The workspace path whose styles the file takes when it is there.
    pub fn reference(self) -> &'static str {
        match self {
            OfficeFormat::Docx => "reference.docx",
            OfficeFormat::Odt => "reference.odt",
        }
    }
}

/// What the `document` build runs, and the probe through it: the HTML
/// typst wrote, as `format`. Typst gives the document's title the `h1`
/// and a level-one heading an `h2`; one level up, the title is the
/// document's and `= Heading` is Heading 1, as the template's author
/// wrote it. Without pandoc's own sandbox, which a distribution's pandoc
/// cannot write these formats under; the helper's HTML loads nothing.
pub fn from_html<'a>(
    format: OfficeFormat,
    html: &'a Path,
    output: &'a Path,
    reference: Option<&'a Path>,
) -> Conversion<'a> {
    let conversion = Conversion::new("html", format.name(), html, output)
        .arg("--shift-heading-level-by=-1")
        .without_own_sandbox();
    match reference {
        Some(reference) => conversion.reference_doc(reference),
        None => conversion,
    }
}
