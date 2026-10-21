//! Splitting a user filename into its stem and extension, the one way the
//! archive dedupe, the representation download name and the sheet name do
//! it.

/// The suffixes of a compression wrapper: `.gz` over `report.csv` leaves
/// `report.csv.gz`, whose extension is `.csv.gz` and not `.gz`.
const COMPRESSION_SUFFIXES: &[&str] = &["gz", "bz2", "xz", "zst", "lz", "lzma", "z", "br"];

/// The longest extension a compression suffix takes with it: `jsonl.gz`
/// and `fastq.gz` are one extension, `report.gz` is `report` plus `.gz`.
const MAX_WRAPPED_EXT_LEN: usize = 5;

/// The stem and the extension of `filename`, the extension with its dot
/// and empty when there is none: `report.pdf` is `("report", ".pdf")`,
/// `README` is `("README", "")`, `.bashrc` is `(".bashrc", "")`. A
/// compression suffix takes the extension before it when that one is one
/// to five ASCII alphanumerics with a letter among them, so that
/// `a.tar.gz` is `("a", ".tar.gz")` and `export.csv.gz` is
/// `("export", ".csv.gz")`, while `my.report.gz` is `("my.report", ".gz")`
/// and `backup.2024.gz` is `("backup.2024", ".gz")`.
pub fn split_extension(filename: &str) -> (&str, &str) {
    let Some((stem, last)) = filename.rsplit_once('.') else {
        return (filename, "");
    };
    if stem.is_empty() {
        return (filename, "");
    }
    if COMPRESSION_SUFFIXES
        .iter()
        .any(|s| last.eq_ignore_ascii_case(s))
        && let Some((inner, wrapped)) = stem.rsplit_once('.')
        && !inner.is_empty()
        && is_wrapped_extension(wrapped)
    {
        return (inner, &filename[inner.len()..]);
    }
    (stem, &filename[stem.len()..])
}

fn is_wrapped_extension(ext: &str) -> bool {
    (1..=MAX_WRAPPED_EXT_LEN).contains(&ext.len())
        && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        && ext.bytes().any(|b| b.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_the_last_dot() {
        assert_eq!(split_extension("report.pdf"), ("report", ".pdf"));
        assert_eq!(split_extension("Report.PDF"), ("Report", ".PDF"));
        assert_eq!(split_extension("photo été.jpg"), ("photo été", ".jpg"));
        assert_eq!(split_extension("my.report.pdf"), ("my.report", ".pdf"));
        assert_eq!(split_extension("README"), ("README", ""));
        assert_eq!(split_extension(".bashrc"), (".bashrc", ""));
        assert_eq!(split_extension("trailing."), ("trailing", "."));
        assert_eq!(split_extension(""), ("", ""));
    }

    #[test]
    fn a_compression_suffix_takes_the_extension_before_it() {
        assert_eq!(split_extension("a.tar.gz"), ("a", ".tar.gz"));
        assert_eq!(split_extension("a.TAR.GZ"), ("a", ".TAR.GZ"));
        assert_eq!(split_extension("a.tar.bz2"), ("a", ".tar.bz2"));
        assert_eq!(split_extension("a.tar.xz"), ("a", ".tar.xz"));
        assert_eq!(split_extension("a.tar.zst"), ("a", ".tar.zst"));
        assert_eq!(split_extension("a.tar.Z"), ("a", ".tar.Z"));
        assert_eq!(split_extension("export.csv.gz"), ("export", ".csv.gz"));
        assert_eq!(split_extension("dump.sql.xz"), ("dump", ".sql.xz"));
        assert_eq!(split_extension("lines.jsonl.gz"), ("lines", ".jsonl.gz"));
        assert_eq!(split_extension("reads.fastq.gz"), ("reads", ".fastq.gz"));
        assert_eq!(split_extension("site.html.br"), ("site", ".html.br"));
        assert_eq!(split_extension("x.7z.gz"), ("x", ".7z.gz"));
        // A triple: the suffix takes one extension, not every one.
        assert_eq!(split_extension("pkg.tar.zst"), ("pkg", ".tar.zst"));
        assert_eq!(split_extension("a.pkg.tar.zst"), ("a.pkg", ".tar.zst"));
    }

    #[test]
    fn a_compression_suffix_leaves_what_is_not_an_extension() {
        assert_eq!(split_extension("my.report.gz"), ("my.report", ".gz"));
        assert_eq!(split_extension("backup.2024.gz"), ("backup.2024", ".gz"));
        assert_eq!(split_extension("photo été.gz"), ("photo été", ".gz"));
        assert_eq!(split_extension("a.b-c.gz"), ("a.b-c", ".gz"));
        assert_eq!(split_extension("a..gz"), ("a.", ".gz"));
        assert_eq!(split_extension("data.gz"), ("data", ".gz"));
        assert_eq!(split_extension(".tar.gz"), (".tar", ".gz"));
        assert_eq!(split_extension("tar.gz"), ("tar", ".gz"));
    }
}
