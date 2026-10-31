//! Compiling a typst workspace into a PDF or into HTML: a directory holding the template,
//! its includes, its images and fonts, and the data of the one document,
//! all written there by the `document` build job. The compiler runs in the
//! `agrafe-render-document` helper, in the sandbox, never in the server,
//! which does not link this crate: typst pulls in image and font parsers,
//! and nothing decodes user files in the API process.
//!
//! The [`Workspace`] is typst's `World` over that directory. Files are
//! resolved through typst's own virtual paths, which cannot escape the
//! root; packages (`@preview/...`) are refused, since the sandbox has no
//! network and the workspace is meant to hold every file a document uses;
//! fonts are every `.ttf` and `.otf` in the workspace plus the compiler's
//! embedded ones, so a template works with nothing declared. A compile
//! error is reported as `file:line:column: message`, one per line, which
//! the job records as its error.
//!
//! The HTML is typst's HTML export, still experimental upstream and
//! behind its `html` feature, which the library here always enables, so
//! that a template is the same program for both targets: `target()`
//! tells them apart, and the `html` module exists under either. It is one
//! self-contained file, images inline as data URIs, which pandoc turns
//! into a DOCX or an ODT. It is written for pandoc, not a browser: the
//! footnotes are reshaped into the markup pandoc reads as notes, and
//! nothing in it refers to a resource outside it. Typst inlines the
//! images it places; an element a template writes itself with
//! `html.elem` that would load something else, a file or a URL, is
//! removed, since pandoc would fetch it into the office file.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jiff::tz::TimeZone;
use typst::diag::{FileError, FileResult, PackageError, Severity, SourceDiagnostic, Warned};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Feature, Features};
use typst::{Library, LibraryExt, World, WorldExt};
use typst_html::{
    HtmlAttr, HtmlAttrs, HtmlDocument, HtmlElement, HtmlNode, HtmlOptions, HtmlTag, attr, tag,
};
use typst_layout::PagedDocument;
use typst_pdf::PdfOptions;

/// The exit code of the helper for a document that does not compile,
/// its diagnostics on stderr: the document's own fault, which the job
/// records and does not retry. Any other failure exits 1 and is the
/// helper's, retried. Mirrored as `DOCUMENT_REJECTED_EXIT` in
/// `agrafe-render`, which cannot link this crate.
pub const REJECTED_EXIT: u8 = 2;

/// What a workspace compiles to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Pages, laid out: a PDF.
    Pdf,
    /// Flow: one HTML file, its images inline.
    Html,
}

/// Why a workspace could not be opened: its root or its main file is not
/// usable before the compiler has looked at anything.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The main file's path is not a valid path inside the workspace.
    #[error("main file `{0}` is not a valid path")]
    Main(String),
    /// The workspace could not be read while gathering its fonts.
    #[error("reading the workspace: {0}")]
    Io(#[from] std::io::Error),
}

/// Why a document did not compile: the diagnostics, errors and warnings,
/// each located in its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// The compiler's diagnostics, in the order it reported them.
    pub diagnostics: Vec<Diagnostic>,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, diagnostic) in self.diagnostics.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{diagnostic}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CompileError {}

/// One diagnostic of the compiler, as the job error shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Whether it is an error or a warning.
    pub severity: Severity,
    /// `path:line:column` of the offending node, when it has one.
    pub location: Option<Location>,
    /// What the compiler says is wrong.
    pub message: String,
    /// The compiler's suggestions for fixing it, if any.
    pub hints: Vec<String>,
}

/// A place in a workspace file, line and column counted from one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The file's path relative to the workspace root.
    pub path: String,
    /// The line, from one.
    pub line: usize,
    /// The column, from one.
    pub column: usize,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(at) = &self.location {
            write!(f, "{}:{}:{}: ", at.path, at.line, at.column)?;
        }
        let severity = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{severity}: {}", self.message)?;
        for hint in &self.hints {
            write!(f, " (hint: {hint})")?;
        }
        Ok(())
    }
}

/// A workspace directory as typst sees it: the project root, with the
/// fonts found in it and the files read from it on demand.
pub struct Workspace {
    root: PathBuf,
    main: FileId,
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    /// Files read so far, by id. The compiler asks for the same file many
    /// times over its passes.
    files: Mutex<HashMap<FileId, FileResult<Bytes>>>,
    sources: Mutex<HashMap<FileId, FileResult<Source>>>,
}

impl Workspace {
    /// Open the workspace at `root` with `main` as its entry file, a path
    /// relative to the root. Fonts are gathered now.
    pub fn open(root: &Path, main: &str) -> Result<Self, OpenError> {
        let vpath = VirtualPath::new(main).map_err(|_| OpenError::Main(main.to_string()))?;
        let main = RootedPath::new(VirtualRoot::Project, vpath).intern();
        let mut fonts = Vec::new();
        collect_fonts(root, &mut fonts)?;
        for data in typst_assets::fonts() {
            fonts.extend(Font::iter(Bytes::new(data)));
        }
        let book = FontBook::from_fonts(&fonts);
        Ok(Self {
            root: root.to_path_buf(),
            main,
            library: LazyHash::new(
                Library::builder()
                    .with_features(Features::from_iter([Feature::Html]))
                    .build(),
            ),
            book: LazyHash::new(book),
            fonts,
            files: Mutex::new(HashMap::new()),
            sources: Mutex::new(HashMap::new()),
        })
    }

    /// How many fonts the compiler may pick from, the workspace's and the
    /// embedded ones.
    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }

    /// Compile the main file and export it in `format`.
    pub fn compile(&self, format: Format) -> Result<Vec<u8>, CompileError> {
        match format {
            Format::Pdf => {
                let Warned { output, warnings } = typst::compile::<PagedDocument>(self);
                let document = output.map_err(|errors| self.diagnostics(&warnings, &errors))?;
                typst_pdf::pdf(&document, &PdfOptions::default())
                    .map_err(|errors| self.diagnostics(&warnings, &errors))
            }
            Format::Html => {
                let Warned { output, warnings } = typst::compile::<HtmlDocument>(self);
                let mut document = output.map_err(|errors| self.diagnostics(&warnings, &errors))?;
                for_pandoc(document.root_mut());
                typst_html::html(&document, &HtmlOptions::default())
                    .map(String::into_bytes)
                    .map_err(|errors| self.diagnostics(&warnings, &errors))
            }
        }
    }

    /// Warnings first, errors last: the sandbox keeps the tail of what the
    /// helper says, and the errors are what matters.
    fn diagnostics(
        &self,
        warnings: &[SourceDiagnostic],
        errors: &[SourceDiagnostic],
    ) -> CompileError {
        CompileError {
            diagnostics: warnings
                .iter()
                .chain(errors)
                .map(|d| self.diagnostic(d))
                .collect(),
        }
    }

    fn diagnostic(&self, diagnostic: &SourceDiagnostic) -> Diagnostic {
        let location = diagnostic.span.id().and_then(|id| {
            let source = self.source(id).ok()?;
            let range = self.range(diagnostic.span)?;
            let (line, column) = source.lines().byte_to_line_column(range.start)?;
            Some(Location {
                path: id.vpath().get_without_slash().to_string(),
                line: line + 1,
                column: column + 1,
            })
        });
        Diagnostic {
            severity: diagnostic.severity,
            location,
            message: diagnostic.message.to_string(),
            hints: diagnostic.hints.iter().map(|h| h.v.to_string()).collect(),
        }
    }

    /// Read a file of the workspace. A package path is refused before any
    /// lookup, and the virtual path is realised under the root by typst,
    /// which refuses what would escape it.
    fn read(&self, id: FileId) -> FileResult<Bytes> {
        if let VirtualRoot::Package(_) = id.root() {
            return Err(FileError::Package(PackageError::Other(Some(
                "packages are not available: a document's workspace holds every file it uses"
                    .into(),
            ))));
        }
        let path = id.vpath().realize(&self.root).map_err(FileError::Realize)?;
        match std::fs::read(&path) {
            Ok(data) => Ok(Bytes::new(data)),
            // The real path is the job's temporary directory; the
            // workspace path is what the template author wrote.
            Err(err) => Err(FileError::from_io(
                err,
                Path::new(id.vpath().get_without_slash()),
            )),
        }
    }
}

impl World for Workspace {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        let mut sources = self.sources.lock().expect("no panic holds the lock");
        sources
            .entry(id)
            .or_insert_with(|| {
                let bytes = self.file(id)?;
                let text = std::str::from_utf8(&bytes).map_err(|_| FileError::InvalidUtf8)?;
                Ok(Source::new(id, text.to_string()))
            })
            .clone()
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        let mut files = self.files.lock().expect("no panic holds the lock");
        files.entry(id).or_insert_with(|| self.read(id)).clone()
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    /// The date in UTC, or at `offset` from it: the sandbox has no local
    /// time zone.
    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        let now = jiff::Timestamp::now();
        let shifted = match offset {
            // An offset a template can write may be beyond any duration.
            Some(offset) => now
                .checked_add(jiff::SignedDuration::try_from_secs_f64(offset.seconds()).ok()?)
                .ok()?,
            None => now,
        };
        let zoned = shifted.to_zoned(TimeZone::UTC);
        Datetime::from_ymd_hms(
            zoned.year().into(),
            zoned.month() as u8,
            zoned.day() as u8,
            zoned.hour() as u8,
            zoned.minute() as u8,
            zoned.second() as u8,
        )
    }
}

/// The HTML as pandoc is to read it, below `element`: no reference to a
/// resource outside it, and the notes in pandoc's shape.
///
/// Typst marks a footnote's reference, and the backlink in front of its
/// text, with a role on a `sup` around the link; pandoc's HTML reader
/// makes a note of them when the role is on a link around the `sup`, the
/// shape its own writer emits. Otherwise the reference is a superscript
/// link and the text of the note is lost.
fn for_pandoc(element: &mut HtmlElement) {
    if element
        .children
        .iter()
        .any(|child| matches!(child, HtmlNode::Element(e) if loads_elsewhere(e)))
    {
        element.children = element
            .children
            .iter()
            .filter(|child| !matches!(child, HtmlNode::Element(e) if loads_elsewhere(e)))
            .cloned()
            .collect();
    }
    for child in element.children.make_mut() {
        if let HtmlNode::Element(child) = child {
            child.attrs = HtmlAttrs(
                child
                    .attrs
                    .0
                    .iter()
                    .filter(|(name, value)| !refers_elsewhere(child.tag, *name, value))
                    .cloned()
                    .collect(),
            );
            if let Some(link) = swapped_note_link(child) {
                *child = link;
            }
            for_pandoc(child);
        }
    }
}

/// Elements whose purpose is to load something, which pandoc would
/// fetch: removed whatever they point at, as are media that point
/// anywhere but at bytes of their own, a `data:` URI.
fn loads_elsewhere(element: &HtmlElement) -> bool {
    const LOADERS: [HtmlTag; 12] = [
        tag::iframe,
        tag::object,
        tag::embed,
        tag::link,
        tag::script,
        tag::base,
        tag::area,
        HtmlTag::constant("frame"),
        HtmlTag::constant("frameset"),
        HtmlTag::constant("portal"),
        HtmlTag::constant("applet"),
        HtmlTag::constant("image"),
    ];
    const MEDIA: [HtmlTag; 6] = [
        tag::img,
        tag::video,
        tag::audio,
        tag::source,
        tag::track,
        tag::input,
    ];
    if LOADERS.contains(&element.tag) {
        return true;
    }
    MEDIA.contains(&element.tag)
        && element
            .attrs
            .get(attr::src)
            .is_some_and(|src| !is_data_uri(src))
}

/// Whether the attribute `name` of a `tag` element points outside the
/// HTML: any URL attribute but a link's own target, which pandoc keeps
/// as a link and never fetches.
fn refers_elsewhere(tag: HtmlTag, name: HtmlAttr, value: &str) -> bool {
    const URLS: [HtmlAttr; 6] = [
        attr::src,
        attr::srcset,
        attr::poster,
        attr::data,
        HtmlAttr::constant("background"),
        HtmlAttr::constant("lowsrc"),
    ];
    let url = URLS.contains(&name) || (name == attr::href && tag != tag::a);
    url && !is_data_uri(value)
}

fn is_data_uri(value: &str) -> bool {
    value
        .trim_start()
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

/// `<sup role="doc-noteref" id><a href>1</a></sup>` as
/// `<a href role="doc-noteref" id><sup>1</sup></a>`, and the same for
/// `doc-backlink`; `None` for any other element.
fn swapped_note_link(sup: &HtmlElement) -> Option<HtmlElement> {
    if sup.tag != tag::sup {
        return None;
    }
    let role = sup.attrs.get(attr::role)?;
    if role != "doc-noteref" && role != "doc-backlink" {
        return None;
    }
    let mut elements = sup.children.iter().filter_map(|node| match node {
        HtmlNode::Element(element) => Some(element),
        _ => None,
    });
    let link = elements.next().filter(|element| element.tag == tag::a)?;
    if elements.next().is_some() {
        return None;
    }
    let (moved, kept): (Vec<_>, Vec<_>) = sup
        .attrs
        .0
        .iter()
        .cloned()
        .partition(|(name, _)| *name == attr::role || *name == attr::id);
    let mut inner = sup.clone();
    inner.attrs = HtmlAttrs(kept.into_iter().collect());
    inner.children = link.children.clone();
    let mut outer = link.clone();
    for (name, value) in moved {
        outer.attrs.push(name, value);
    }
    outer.children = std::iter::once(HtmlNode::Element(inner)).collect();
    Some(outer)
}

/// Every `.ttf` and `.otf` under `root`, in path order so the font book is
/// the same whatever the directory listing order.
fn collect_fonts(root: &Path, fonts: &mut Vec<Font>) -> std::io::Result<()> {
    let mut paths = Vec::new();
    walk(root, &mut paths)?;
    paths.sort();
    for path in paths {
        let is_font = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ttf") || ext.eq_ignore_ascii_case("otf"));
        if is_font {
            let data = std::fs::read(&path)?;
            fonts.extend(Font::iter(Bytes::new(data)));
        }
    }
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pixel PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn open(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        for (path, bytes) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let workspace = Workspace::open(dir.path(), "main.typ").unwrap();
        (dir, workspace)
    }

    #[test]
    fn compiles_a_workspace_of_includes_data_and_images() {
        let (_dir, workspace) = open(&[
            (
                "main.typ",
                b"#import \"lib/header.typ\": title\n#let d = json(\"data.json\")\n#title(d.nom)\n#image(\"img/logo.png\", width: 1cm)\nAujourd'hui: #datetime.today().display()",
            ),
            ("lib/header.typ", b"#let title(t) = heading(t)"),
            ("data.json", br#"{"nom": "Facture n\u00b0 12"}"#),
            ("img/logo.png", PNG),
        ]);
        let pdf = workspace.compile(Format::Pdf).unwrap();
        assert!(pdf.starts_with(b"%PDF-"), "{:?}", &pdf[..8]);
    }

    #[test]
    fn compiles_to_one_html_file_of_structure() {
        let (_dir, workspace) = open(&[
            (
                "main.typ",
                b"#set page(width: 10cm)\n= Titre\n- un\n- deux\n\n#table(columns: 2, [a], [b])\n#image(\"pixel.png\", alt: \"point\")\n$ x^2 $\n#context if target() == \"html\" [flow] else [pages]#footnote[note]",
            ),
            ("pixel.png", PNG),
        ]);
        let html = String::from_utf8(workspace.compile(Format::Html).unwrap()).unwrap();
        for element in [
            "<h2>Titre</h2>",
            "<ul>",
            "<li>un</li>",
            "<table>",
            "<img src=\"data:image/png;base64,",
            "<math",
            "flow",
            // A note in the markup pandoc's reader knows.
            "role=\"doc-noteref\"><sup>1</sup></a>",
            "role=\"doc-backlink\"><sup>1</sup></a>",
        ] {
            assert!(html.contains(element), "{element} in {html}");
        }
        assert!(!html.contains("pages"), "{html}");
        // The same template as pages, where `target()` says so.
        let pdf = workspace.compile(Format::Pdf).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
    }

    /// What a template writes with `html.elem` loads nothing pandoc
    /// would fetch: a file of the worker's, a URL.
    #[test]
    fn html_refers_to_nothing_outside_it() {
        let (_dir, workspace) = open(&[
            (
                "main.typ",
                br##"#html.elem("img", attrs: (src: "/etc/passwd"))
#html.elem("img", attrs: (src: " DATA:image/png;base64,AA=="))
#html.elem("iframe", attrs: (src: "file:///etc/passwd"))
#html.elem("video", attrs: (src: "https://example.org/v.mp4", poster: "/etc/hosts"))
#html.elem("div", attrs: (background: "/etc/hosts"))[kept]
#html.elem("object", attrs: (data: "/etc/passwd"))
#html.elem("span", attrs: (href: "/etc/passwd"))[span]
#link("file:///etc/passwd")[link]
#html.elem("sup", attrs: (role: "doc-noteref"))[#html.elem("a", attrs: (href: "#n"))[#html.elem("img", attrs: (src: "/etc/shadow"))]]
#image("pixel.png")"##,
            ),
            ("pixel.png", PNG),
        ]);
        let html = String::from_utf8(workspace.compile(Format::Html).unwrap()).unwrap();
        for gone in [
            "iframe",
            "video",
            "object",
            "/etc/hosts",
            "/etc/shadow",
            "src=\"/etc/passwd\"",
        ] {
            assert!(!html.contains(gone), "{gone} in {html}");
        }
        for kept in [
            "src=\" DATA:image/png;base64,AA==\"",
            "<div>kept</div>",
            "<span>span</span>",
            // A link is a link: pandoc keeps it and fetches nothing.
            "<a href=\"file:///etc/passwd\">link</a>",
            "<img src=\"data:image/png;base64,",
        ] {
            assert!(html.contains(kept), "{kept} in {html}");
        }
    }

    #[test]
    fn html_errors_are_located() {
        let (_dir, workspace) = open(&[("main.typ", b"= Titre\n#nope")]);
        let text = workspace.compile(Format::Html).unwrap_err().to_string();
        assert!(text.contains("main.typ:2:"), "{text}");
    }

    #[test]
    fn errors_are_located() {
        let (_dir, workspace) = open(&[("main.typ", b"= Titre\n#let x = \n#nope")]);
        let err = workspace.compile(Format::Pdf).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("main.typ:2:"), "{text}");
        assert!(text.contains("error:"), "{text}");
    }

    #[test]
    fn missing_main_is_reported() {
        let (_dir, workspace) = open(&[("other.typ", b"")]);
        let text = workspace.compile(Format::Pdf).unwrap_err().to_string();
        assert!(text.contains("main.typ"), "{text}");
        assert!(text.contains("not found"), "{text}");
    }

    #[test]
    fn packages_are_refused() {
        let (_dir, workspace) =
            open(&[("main.typ", b"#import \"@preview/example:0.1.0\": *\nhello")]);
        let text = workspace.compile(Format::Pdf).unwrap_err().to_string();
        assert!(text.contains("packages are not available"), "{text}");
    }

    #[test]
    fn the_root_cannot_be_escaped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret.typ"), b"= Secret").unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("main.typ"), b"#include \"../secret.typ\"").unwrap();
        let workspace = Workspace::open(&root, "main.typ").unwrap();
        let text = workspace.compile(Format::Pdf).unwrap_err().to_string();
        assert!(
            text.contains("not found") || text.contains("escape"),
            "{text}"
        );
    }

    #[test]
    fn workspace_fonts_join_the_embedded_ones() {
        let embedded = {
            let (_dir, workspace) = open(&[("main.typ", b"x" as &[u8])]);
            workspace.font_count()
        };
        assert!(embedded > 0);
        let data = typst_assets::fonts().next().unwrap();
        let (_dir, workspace) = open(&[("main.typ", b"x" as &[u8]), ("fonts/Extra.OTF", data)]);
        assert!(workspace.font_count() > embedded);
        let (_dir, workspace) = open(&[("main.typ", b"x" as &[u8]), ("fonts/Extra.txt", data)]);
        assert_eq!(workspace.font_count(), embedded, "by extension");
    }

    #[test]
    fn main_may_live_in_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/doc.typ"), b"= Hi").unwrap();
        let workspace = Workspace::open(dir.path(), "src/doc.typ").unwrap();
        assert!(workspace.compile(Format::Pdf).is_ok());
        assert!(Workspace::open(dir.path(), "a\\b.typ").is_err());
    }
}
