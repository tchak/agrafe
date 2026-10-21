//! The workspace's `Cargo.toml`, read and edited as a document so that its
//! comments and layout survive a bump.

use semver::Version;
use toml_edit::{DocumentMut, Item, TableLike, Value};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("{0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("no {0}")]
    Missing(&'static str),
    #[error("workspace.package.version: {0}")]
    Version(semver::Error),
    #[error("workspace.dependencies.{name} requires {found}, not the workspace's {expected}")]
    Requirement {
        name: String,
        found: String,
        expected: Version,
    },
}

fn package(doc: &DocumentMut) -> Result<&dyn TableLike, ManifestError> {
    doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(Item::as_table_like)
        .ok_or(ManifestError::Missing("[workspace.package]"))
}

/// `workspace.package.version`, every crate's.
pub fn version(manifest: &str) -> Result<Version, ManifestError> {
    let doc: DocumentMut = manifest.parse()?;
    let version = package(&doc)?
        .get("version")
        .and_then(Item::as_str)
        .ok_or(ManifestError::Missing("workspace.package.version"))?;
    Version::parse(version).map_err(ManifestError::Version)
}

/// `workspace.package.repository`, without a trailing slash.
pub fn repository(manifest: &str) -> Result<String, ManifestError> {
    let doc: DocumentMut = manifest.parse()?;
    package(&doc)?
        .get("repository")
        .and_then(Item::as_str)
        .map(|r| r.trim_end_matches('/').to_owned())
        .ok_or(ManifestError::Missing("workspace.package.repository"))
}

/// `manifest` with the workspace's version, and the version requirement
/// of each crate of the workspace in `[workspace.dependencies]` (those with
/// a `path`), moved from `from` to `to`.
pub fn bump(manifest: &str, from: &Version, to: &Version) -> Result<String, ManifestError> {
    let mut doc: DocumentMut = manifest.parse()?;
    let to_string = to.to_string();
    let package = doc
        .get_mut("workspace")
        .and_then(|w| w.get_mut("package"))
        .and_then(Item::as_table_like_mut)
        .ok_or(ManifestError::Missing("[workspace.package]"))?;
    let version = package
        .get_mut("version")
        .and_then(Item::as_value_mut)
        .ok_or(ManifestError::Missing("workspace.package.version"))?;
    set(version, &to_string);

    let dependencies = doc
        .get_mut("workspace")
        .and_then(|w| w.get_mut("dependencies"))
        .and_then(Item::as_table_like_mut)
        .ok_or(ManifestError::Missing("[workspace.dependencies]"))?;
    for (name, dependency) in dependencies.iter_mut() {
        let Some(dependency) = dependency.as_table_like_mut() else {
            continue;
        };
        if !dependency.contains_key("path") {
            continue;
        }
        let requirement = dependency
            .get_mut("version")
            .and_then(Item::as_value_mut)
            .ok_or(ManifestError::Missing("the version of a path dependency"))?;
        let found = requirement.as_str().unwrap_or_default().to_owned();
        if found != from.to_string() {
            return Err(ManifestError::Requirement {
                name: name.to_string(),
                found,
                expected: from.clone(),
            });
        }
        set(requirement, &to_string);
    }
    Ok(doc.to_string())
}

/// Replaces a string value, keeping the spaces and comment around it.
fn set(value: &mut Value, to: &str) {
    let decor = value.decor().clone();
    *value = to.into();
    *value.decor_mut() = decor;
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"[workspace]
members = ["crates/a", "crates/b"]

[workspace.package]
# Every crate's.
version = "0.1.0"  # bumped by xtask
repository = "https://github.com/tchak/agrafe/"

[workspace.dependencies]
agrafe-a = { version = "0.1.0", path = "crates/a" }
agrafe-b = { version = "0.1.0", path = "crates/b", features = ["x"] }
serde = { version = "0.1.0" }
"#;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn reads_the_version_and_the_repository() {
        assert_eq!(version(MANIFEST).unwrap(), v("0.1.0"));
        assert_eq!(
            repository(MANIFEST).unwrap(),
            "https://github.com/tchak/agrafe"
        );
    }

    #[test]
    fn bumps_the_version_and_the_internal_requirements_alone() {
        let bumped = bump(MANIFEST, &v("0.1.0"), &v("0.2.0-rc.1")).unwrap();
        assert_eq!(
            bumped,
            MANIFEST
                .replace(r#"version = "0.1.0"  #"#, r#"version = "0.2.0-rc.1"  #"#)
                .replace(
                    r#"agrafe-a = { version = "0.1.0""#,
                    r#"agrafe-a = { version = "0.2.0-rc.1""#
                )
                .replace(
                    r#"agrafe-b = { version = "0.1.0""#,
                    r#"agrafe-b = { version = "0.2.0-rc.1""#
                )
        );
        assert_eq!(version(&bumped).unwrap(), v("0.2.0-rc.1"));
    }

    #[test]
    fn an_internal_requirement_off_the_version_is_refused() {
        let drifted = MANIFEST.replace(
            r#"agrafe-b = { version = "0.1.0""#,
            r#"agrafe-b = { version = "0.0.9""#,
        );
        let err = bump(&drifted, &v("0.1.0"), &v("0.2.0")).unwrap_err();
        assert!(
            matches!(&err, ManifestError::Requirement { name, .. } if name == "agrafe-b"),
            "{err}"
        );
    }

    #[test]
    fn a_manifest_without_the_workspace_version_is_refused() {
        let err = version("[workspace]\n").unwrap_err();
        assert!(matches!(err, ManifestError::Missing(_)), "{err}");
    }
}
