//! What must not leave the process, taken out of any text on its way to an
//! exporter. The spans and events are written to hold no secret; this is
//! the line behind that, for what an error message or a future field
//! brings along. A collector's or Sentry's own scrubbing comes too late:
//! the data has left by then.

use std::borrow::Cow;

/// The field of a job's failure, which may quote what a user's file or a
/// host's manifest says: a compiler's diagnostics, a tool's stderr. No
/// pattern finds that, so it is dropped whole; it stays in the logs and on
/// the job's row, where the event's `trace_id` leads.
pub const LAST_ERROR: &str = "last_error";

/// What a word ends on in a message. Not `<`, `>`, `{` or `}`, which are in
/// `/blobs/<token>` and `/blobs/{token}`, the forms that are clean already.
fn is_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']')
}

/// `text` with public paths and URLs defused: the token segment of a
/// `/blobs/` path, and what follows it, which holds the filename, becomes
/// `<token>`, however the path is prefixed; a token alone becomes
/// `<token>` too; a URL or path loses its query string, where a sealed
/// read's envelope is. A blob's id in a private path stays: it is what
/// makes an error findable.
pub fn scrub(text: &str) -> Cow<'_, str> {
    let mut scrubbed: Option<String> = None;
    let mut clean_to = 0;
    let words = text
        .split(is_separator)
        .map(|word| (word.as_ptr() as usize - text.as_ptr() as usize, word));
    for (start, word) in words {
        if let Cow::Owned(word_scrubbed) = scrub_word(word) {
            let out = scrubbed.get_or_insert_default();
            out.push_str(&text[clean_to..start]);
            out.push_str(&word_scrubbed);
            clean_to = start + word.len();
        }
    }
    match scrubbed {
        Some(mut out) => {
            out.push_str(&text[clean_to..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(text),
    }
}

/// The word defused, borrowed as it was when there is nothing to take out.
fn scrub_word(word: &str) -> Cow<'_, str> {
    let mut text = Cow::Borrowed(word);
    if let Some(token) = token_start(&text) {
        text = Cow::Owned(format!("{}<token>", &text[..token]));
    }
    let mut from = 0;
    while let Some(found) = text[from..].find("/blobs/") {
        let segment_at = from + found + "/blobs/".len();
        let segment = text[segment_at..]
            .split(['/', '?'])
            .next()
            .unwrap_or_default();
        if segment.starts_with(['<', '{']) || is_uuid(segment) {
            from = segment_at;
        } else {
            text = Cow::Owned(format!("{}<token>", &text[..segment_at]));
            break;
        }
    }
    if let Some(query) = text.find('?')
        && (text.contains("://") || text.starts_with('/') || text[query..].contains('='))
    {
        text = Cow::Owned(text[..query].to_string());
    }
    text
}

/// Where a token starts in a word, alone or as a value, `token=v1.…`: it is
/// `v1.<key_id>.<expires_unix>.<signature>`, see `agrafe::signing`, and its
/// signature ends the word. A version number shaped the same goes too,
/// which costs nothing.
fn token_start(word: &str) -> Option<usize> {
    word.match_indices("v1.").map(|(at, _)| at).find(|&at| {
        let at_boundary = word[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_ascii_alphanumeric());
        let mut fields = word[at + "v1.".len()..].split('.');
        let (key, expires, signature) = (fields.next(), fields.next(), fields.next());
        at_boundary
            && key.is_some_and(|key| !key.is_empty())
            && expires.is_some_and(|expires| {
                !expires.is_empty() && expires.bytes().all(|b| b.is_ascii_digit())
            })
            && signature.is_some_and(|signature| !signature.is_empty())
    })
}

fn is_uuid(segment: &str) -> bool {
    segment.len() == 36
        && segment.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "v1.k1.2000000000.c2lnbmF0dXJl";
    const ID: &str = "0190b5a2-1c3d-7e4f-8a9b-0c1d2e3f4a5b";

    #[test]
    fn takes_the_token_and_the_filename_out_of_a_path() {
        for (text, clean) in [
            (
                format!("/blobs/{TOKEN}/{ID}/original/secret.pdf"),
                "/blobs/<token>".to_string(),
            ),
            (format!("/blobs/{TOKEN}"), "/blobs/<token>".to_string()),
            ("/blobs/".to_string(), "/blobs/<token>".to_string()),
            (
                format!("GET /files/blobs/{TOKEN}/{ID}/original/secret.pdf failed: 500"),
                "GET /files/blobs/<token> failed: 500".to_string(),
            ),
            (
                format!("uri=\"https://agrafe.test/blobs/{TOKEN}/{ID}/x.png?e=abc\", then"),
                "uri=\"https://agrafe.test/blobs/<token>\", then".to_string(),
            ),
            (
                format!("signed by {TOKEN}."),
                "signed by <token>".to_string(),
            ),
            (format!("token={TOKEN}"), "token=<token>".to_string()),
        ] {
            assert_eq!(scrub(&text), clean, "{text}");
        }
    }

    #[test]
    fn drops_query_strings() {
        for (text, clean) in [
            (
                "https://s3.test/key?X-Amz-Signature=abc",
                "https://s3.test/key",
            ),
            (
                "fetching https://s3.test/key?a b",
                "fetching https://s3.test/key b",
            ),
            ("/blobs?limit=5", "/blobs"),
            ("target=key?e=abc done", "target=key done"),
        ] {
            assert_eq!(scrub(text), clean, "{text}");
        }
    }

    #[test]
    fn leaves_what_is_clean_alone() {
        for text in [
            "/blobs/<token>/x",
            "/blobs/{token}/{id}/original/{filename}",
            "/healthz",
            "what now? nothing",
            "the database: connection refused",
            "",
        ] {
            assert!(matches!(scrub(text), Cow::Borrowed(_)), "{text}");
            assert_eq!(scrub(text), text);
        }
        // A blob's id in a private path is what finds the error.
        let private = format!("/blobs/{ID}/retry");
        assert_eq!(scrub(&private), private);
    }

    #[test]
    fn a_scrubbed_text_stays_as_it_is() {
        let once = scrub(&format!("/blobs/{TOKEN}/{ID}/a.pdf?e=1")).into_owned();
        assert_eq!(scrub(&once), once);
    }
}
