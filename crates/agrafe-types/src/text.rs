//! What the two sides of the text thumbnail agree on: how much of a file
//! the server hands the `agrafe-render-text` helper, and which characters
//! are invisible formatting that neither a filename nor a rendered sample
//! may carry.

/// Bytes of a text file the helper is handed, and the most it reads of its
/// input; more would not fit on its page. The sample is the file's first
/// bytes as they are, with no marker for a cut: the helper draws its first
/// lines only, so neither side adds one.
pub const SAMPLE_LEN: u64 = 8 * 1024;

/// Bidirectional formatting characters. `char::is_control` covers only the
/// C0 and C1 controls, and these can make `invoice\u{202E}fdp.exe` display
/// as `invoiceexe.pdf` in a download prompt, a URL bar or a thumbnail.
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}
