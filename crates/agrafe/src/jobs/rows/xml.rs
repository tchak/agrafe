//! Writing XML by hand: escaping, and the characters XML 1.0 cannot carry.

/// Append `s` as element content: `&`, `<` and `>` escaped, characters
/// XML 1.0 forbids replaced.
pub fn push_text(out: &mut Vec<u8>, s: &str) {
    push_escaped(out, s, false);
}

/// Append `s` as an attribute value: as content, plus `"` and the
/// whitespace an attribute would otherwise lose.
pub fn push_attr(out: &mut Vec<u8>, s: &str) {
    push_escaped(out, s, true);
}

fn push_escaped(out: &mut Vec<u8>, s: &str, attribute: bool) {
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match c {
            '&' => out.extend_from_slice(b"&amp;"),
            '<' => out.extend_from_slice(b"&lt;"),
            '>' => out.extend_from_slice(b"&gt;"),
            '"' if attribute => out.extend_from_slice(b"&quot;"),
            '\n' if attribute => out.extend_from_slice(b"&#10;"),
            '\r' if attribute => out.extend_from_slice(b"&#13;"),
            '\t' if attribute => out.extend_from_slice(b"&#9;"),
            c if !is_xml_char(c) => out.extend_from_slice("\u{FFFD}".as_bytes()),
            c => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
}

/// Whether XML 1.0 allows `c` in a document at all: tab, newline and
/// carriage return, then everything from space on but the two
/// non-characters. A control character in a cell cannot show anyway.
pub fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{FFFE}' && c != '\u{FFFF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> String {
        let mut out = Vec::new();
        push_text(&mut out, s);
        String::from_utf8(out).unwrap()
    }

    fn attr(s: &str) -> String {
        let mut out = Vec::new();
        push_attr(&mut out, s);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn escapes() {
        assert_eq!(text("a < b & c > \"d\"\n"), "a &lt; b &amp; c &gt; \"d\"\n");
        assert_eq!(
            attr("a < b & \"d\"\n\t"),
            "a &lt; b &amp; &quot;d&quot;&#10;&#9;"
        );
        assert_eq!(
            text("x\u{0}y\u{1F}z\u{FFFF}"),
            "x\u{FFFD}y\u{FFFD}z\u{FFFD}"
        );
        assert_eq!(text("été\t\r\n"), "été\t\r\n");
    }
}
