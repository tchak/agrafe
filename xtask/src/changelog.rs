//! `CHANGELOG.md`, in Keep a Changelog's shape: a `## [Unreleased]`
//! section, then `## [<version>] - <date>` sections, then the link
//! definitions, `[Unreleased]: <url>` first.

use semver::Version;

#[derive(Debug, thiserror::Error)]
pub enum ChangelogError {
    #[error("no section for {0}")]
    NoSection(String),
    #[error("nothing under {0}")]
    Empty(String),
    #[error("no [Unreleased] link")]
    NoLink,
}

const UNRELEASED: &str = "Unreleased";

/// Whether `line` is a link definition, `[name]: url`.
fn is_link(line: &str) -> bool {
    line.starts_with('[') && line.contains("]: ")
}

/// What the section named `name` says, up to the next section or the
/// links, trimmed.
fn section<'a>(changelog: &'a str, name: &str) -> Result<&'a str, ChangelogError> {
    let heading = format!("## [{name}]");
    let mut lines = changelog.split_inclusive('\n');
    let mut start = None;
    let mut offset = 0;
    for line in lines.by_ref() {
        offset += line.len();
        if line.starts_with(&heading) {
            start = Some(offset);
            break;
        }
    }
    let start = start.ok_or_else(|| ChangelogError::NoSection(name.into()))?;
    let mut end = start;
    for line in lines {
        if line.starts_with("## [") || is_link(line) {
            break;
        }
        end += line.len();
    }
    let body = changelog[start..end].trim();
    if body.is_empty() {
        return Err(ChangelogError::Empty(name.into()));
    }
    Ok(body)
}

/// What Unreleased says, refusing an empty section.
pub fn unreleased(changelog: &str) -> Result<&str, ChangelogError> {
    section(changelog, UNRELEASED)
}

/// A version's release notes: its section, or Unreleased for a
/// pre-release, which gets none.
pub fn notes<'a>(changelog: &'a str, version: &Version) -> Result<&'a str, ChangelogError> {
    if version.pre.is_empty() {
        section(changelog, &version.to_string())
    } else {
        unreleased(changelog)
    }
}

/// `changelog` with Unreleased's entries moved under `version`, dated
/// `date`, an empty Unreleased above it, and the links pointed at the
/// version's tag: Unreleased compares from it, and the version against
/// `last_release` or, for the first, is the tag's release page.
pub fn release(
    changelog: &str,
    version: &Version,
    date: &str,
    repository: &str,
    last_release: Option<&Version>,
) -> Result<String, ChangelogError> {
    unreleased(changelog)?;
    let heading = format!("## [{UNRELEASED}]\n");
    let tag = format!("v{version}");
    let link = match last_release {
        Some(last) => format!("{repository}/compare/v{last}...{tag}"),
        None => format!("{repository}/releases/tag/{tag}"),
    };
    let unreleased_link = format!("[{UNRELEASED}]: ");

    let mut out = String::with_capacity(changelog.len() + 200);
    let mut linked = false;
    for line in changelog.split_inclusive('\n') {
        if line == heading {
            out.push_str(&heading);
            out.push_str(&format!("\n## [{version}] - {date}\n"));
        } else if line.starts_with(&unreleased_link) && !linked {
            out.push_str(&format!(
                "{unreleased_link}{repository}/compare/{tag}...HEAD\n[{version}]: {link}\n"
            ));
            linked = true;
        } else {
            out.push_str(line);
        }
    }
    if !linked {
        return Err(ChangelogError::NoLink);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOSITORY: &str = "https://github.com/tchak/agrafe";

    const FIRST: &str = "# Changelog

Intro.

## [Unreleased]

### Added

- Everything.

[Unreleased]: https://github.com/tchak/agrafe/commits/main
";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn the_first_release_takes_unreleased_and_links_its_tag() {
        let out = release(FIRST, &v("0.1.0"), "2026-10-21", REPOSITORY, None).unwrap();
        assert_eq!(
            out,
            "# Changelog

Intro.

## [Unreleased]

## [0.1.0] - 2026-10-21

### Added

- Everything.

[Unreleased]: https://github.com/tchak/agrafe/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/tchak/agrafe/releases/tag/v0.1.0
"
        );
        assert_eq!(
            notes(&out, &v("0.1.0")).unwrap(),
            "### Added\n\n- Everything."
        );
        assert!(matches!(unreleased(&out), Err(ChangelogError::Empty(_))));
    }

    #[test]
    fn a_later_release_compares_against_the_last_release() {
        let first = release(FIRST, &v("0.1.0"), "2026-10-21", REPOSITORY, None).unwrap();
        let next = first.replace(
            "## [Unreleased]\n\n## [0.1.0]",
            "## [Unreleased]\n\n### Fixed\n\n- One.\n\n## [0.1.0]",
        );
        let out = release(
            &next,
            &v("0.2.0"),
            "2026-11-02",
            REPOSITORY,
            Some(&v("0.1.0")),
        )
        .unwrap();
        assert!(out.contains("## [Unreleased]\n\n## [0.2.0] - 2026-11-02\n\n### Fixed\n\n- One.\n\n## [0.1.0] - 2026-10-21\n"));
        assert!(out.ends_with(
            "[Unreleased]: https://github.com/tchak/agrafe/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/tchak/agrafe/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tchak/agrafe/releases/tag/v0.1.0
"
        ));
        // The section stops at the next one.
        assert_eq!(notes(&out, &v("0.2.0")).unwrap(), "### Fixed\n\n- One.");
    }

    #[test]
    fn a_pre_release_s_notes_are_unreleased() {
        assert_eq!(
            notes(FIRST, &v("0.1.0-rc.1")).unwrap(),
            "### Added\n\n- Everything."
        );
    }

    #[test]
    fn a_missing_or_empty_section_is_refused() {
        assert!(matches!(
            notes(FIRST, &v("0.3.0")),
            Err(ChangelogError::NoSection(_))
        ));
        let empty = FIRST.replace("### Added\n\n- Everything.\n\n", "");
        assert!(matches!(
            release(&empty, &v("0.1.0"), "2026-10-21", REPOSITORY, None),
            Err(ChangelogError::Empty(_))
        ));
        let unlinked = FIRST.replace(
            "[Unreleased]: https://github.com/tchak/agrafe/commits/main\n",
            "",
        );
        assert!(matches!(
            release(&unlinked, &v("0.1.0"), "2026-10-21", REPOSITORY, None),
            Err(ChangelogError::NoLink)
        ));
    }
}
