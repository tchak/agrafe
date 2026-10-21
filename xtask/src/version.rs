//! The versions already released, from the repository's tags. Ordering is
//! `semver::Version`'s, which is semver's precedence: a pre-release before
//! its release, identifiers numerically or lexically as the spec says.

use semver::Version;

/// The versions of the `v<version>` tags in `tags`, one name a line,
/// sorted from the oldest; a tag that is not a version is ignored.
pub fn tagged(tags: &str) -> Vec<Version> {
    let mut versions: Vec<Version> = tags
        .lines()
        .filter_map(|tag| tag.trim().strip_prefix('v'))
        .filter_map(|v| Version::parse(v).ok())
        .collect();
    versions.sort();
    versions.dedup();
    versions
}

/// The last of `versions`, sorted, that is not a pre-release.
pub fn last_release(versions: &[Version]) -> Option<&Version> {
    versions.iter().rev().find(|v| v.pre.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn tags_sort_by_semver_precedence() {
        let tags =
            "v0.2.0\nv0.10.0-rc.1\nv0.2.0-rc.10\nv0.2.0-rc.2\nv0.1.0\nlatest\nv0.2\nv0.2.0\n";
        let versions: Vec<String> = tagged(tags).iter().map(Version::to_string).collect();
        assert_eq!(
            versions,
            ["0.1.0", "0.2.0-rc.2", "0.2.0-rc.10", "0.2.0", "0.10.0-rc.1"]
        );
    }

    #[test]
    fn the_last_release_skips_pre_releases() {
        let versions = tagged("v0.1.0\nv0.2.0-rc.1\nv0.2.0-rc.2\n");
        assert_eq!(last_release(&versions), Some(&v("0.1.0")));
        assert_eq!(last_release(&tagged("v0.1.0-rc.1\n")), None);
        assert_eq!(last_release(&[]), None);
    }
}
