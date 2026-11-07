//! The book's preprocessor, `cargo xtask mdbook`, which mdbook runs on
//! every build (`docs/book.toml`): it fills in the placeholders for the
//! workspace's version, so that the image tags and the version examples
//! the book quotes are the release's own and nothing bumps them by hand.
//! The book is deployed from the release's tag, where the manifest
//! carries that version.
//!
//! A placeholder is a name in double braces, `{{version}}`, with nothing
//! else inside: an unknown name fails the build rather than ships, and
//! what is not a bare name (Prometheus' `{{ $labels.kind }}` in the alert
//! rules the book includes) is not a placeholder.

use semver::Version;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum BookError {
    #[error("reading what mdbook sent: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mdbook sent no book: expected [context, book]")]
    Shape,
    #[error("{chapter}: no placeholder {{{{{name}}}}}; the book knows {known}")]
    Unknown {
        chapter: String,
        name: String,
        known: String,
    },
}

/// The placeholders and what they stand for, at `version`.
pub fn values(version: &Version) -> [(&'static str, String); 4] {
    let (major, minor, patch) = (version.major, version.minor, version.patch);
    [
        // 0.1.0: the release, its crates and its image's own tag.
        ("version", version.to_string()),
        // 0.1: the image tag that follows the newest release of the minor.
        ("series", format!("{major}.{minor}")),
        // 0.2.0: the next minor version, which may break before 1.0.
        ("next_minor", Version::new(major, minor + 1, 0).to_string()),
        // 0.1.1: the next patch version, which breaks nothing.
        (
            "next_patch",
            Version::new(major, minor, patch + 1).to_string(),
        ),
    ]
}

/// `text` with every placeholder filled in, or the first unknown one.
pub fn substitute(text: &str, chapter: &str, version: &Version) -> Result<String, BookError> {
    let values = values(version);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_len];
        if name.is_empty() || !after[name_len..].starts_with("}}") {
            // Not a placeholder: keep the braces and look past them.
            out.push_str("{{");
            rest = after;
            continue;
        }
        let value = values
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v)
            .ok_or_else(|| BookError::Unknown {
                chapter: chapter.to_owned(),
                name: name.to_owned(),
                known: values
                    .iter()
                    .map(|(n, _)| format!("{{{{{n}}}}}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
        out.push_str(value);
        rest = &after[name_len + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// What mdbook sends a preprocessor, `[context, book]`, to the book it
/// expects back, every chapter's content filled in.
pub fn preprocess(input: &str, version: &Version) -> Result<String, BookError> {
    let input: Value = serde_json::from_str(input)?;
    let mut book = match input {
        Value::Array(mut parts) if parts.len() == 2 => parts.pop().ok_or(BookError::Shape)?,
        _ => return Err(BookError::Shape),
    };
    fill(&mut book, version)?;
    Ok(serde_json::to_string(&book)?)
}

/// Every chapter under `value`, at any depth: a chapter is the one object
/// with a `content` string, its sub-chapters among its fields.
fn fill(value: &mut Value, version: &Version) -> Result<(), BookError> {
    match value {
        Value::Object(map) => {
            let chapter = map
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("the book")
                .to_owned();
            for (key, field) in map.iter_mut() {
                match field {
                    Value::String(text) if key == "content" => {
                        *text = substitute(text, &chapter, version)?;
                    }
                    _ => fill(field, version)?,
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                fill(item, version)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn fills_in_the_versions_of_a_release() {
        let text = "ghcr.io/tchak/agrafe:{{version}}-office, :{{series}}, \
                    then {{next_minor}} or {{next_patch}}";
        assert_eq!(
            substitute(text, "Deployment", &v("0.1.0")).unwrap(),
            "ghcr.io/tchak/agrafe:0.1.0-office, :0.1, then 0.2.0 or 0.1.1"
        );
        assert_eq!(
            substitute("{{series}} {{next_minor}}", "", &v("1.4.2")).unwrap(),
            "1.4 1.5.0"
        );
    }

    #[test]
    fn leaves_what_is_not_a_placeholder() {
        for text in [
            "summary: \"agrafe {{ $labels.kind }} jobs\"",
            "{{#include ../../CHANGELOG.md}}",
            "{{Version}} {{}} {{version",
            "a } and a {",
        ] {
            assert_eq!(substitute(text, "", &v("0.1.0")).unwrap(), text);
        }
    }

    #[test]
    fn refuses_an_unknown_placeholder() {
        let err = substitute("agrafe {{verison}}", "Deployment", &v("0.1.0")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Deployment: no placeholder {{verison}}; the book knows {{version}}, \
             {{series}}, {{next_minor}}, {{next_patch}}"
        );
    }

    #[test]
    fn fills_every_chapter_of_the_book() {
        let input = r#"[
            {"root": "/agrafe/docs", "mdbook_version": "0.5.4"},
            {"items": [
                {"Chapter": {"name": "Guide", "content": "agrafe {{version}}",
                    "sub_items": [
                        {"Chapter": {"name": "Deployment", "content": ":{{series}}",
                            "sub_items": []}}
                    ]}},
                "Separator",
                {"PartTitle": "Reference"}
            ]}
        ]"#;
        let book: Value = serde_json::from_str(&preprocess(input, &v("0.3.1")).unwrap()).unwrap();
        let guide = &book["items"][0]["Chapter"];
        assert_eq!(guide["content"], "agrafe 0.3.1");
        assert_eq!(guide["sub_items"][0]["Chapter"]["content"], ":0.3");
        assert_eq!(book["items"][1], "Separator");
    }

    #[test]
    fn names_the_chapter_of_an_unknown_placeholder() {
        let input = r#"[{}, {"items": [
            {"Chapter": {"name": "Webhooks", "content": "{{nope}}", "sub_items": []}}
        ]}]"#;
        assert!(
            preprocess(input, &v("0.1.0"))
                .unwrap_err()
                .to_string()
                .starts_with("Webhooks: no placeholder {{nope}}")
        );
    }
}
