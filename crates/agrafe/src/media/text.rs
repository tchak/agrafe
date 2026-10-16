//! Thumbnails for text files: the first lines of the file set in an
//! embedded monospace font and rasterised here, in Rust, so the result does
//! not depend on the fonts, fontconfig or shaping libraries of the host.
//! The bytes are untrusted, so this runs in the `agrafe render-text`
//! helper, in the sandbox. The output is a PGM for vips to turn into the
//! WebP like any other raster.

use std::borrow::Cow;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

const FONT: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono.ttf");

/// Bytes of the file looked at; more would not fit on the page.
pub const SAMPLE_LEN: u64 = 8 * 1024;

/// Characters per line and lines per page.
const COLUMNS: usize = 32;
const ROWS: usize = 16;

/// Share of control characters above which the sample is not text.
const MAX_CONTROL_RATIO: f32 = 0.02;

/// Width of the page relative to its height: A4.
const PAGE_RATIO: f32 = 0.707;

const BACKGROUND: u8 = 0xe5;
const PAGE: u8 = 0xff;
const INK: u8 = 0x30;

/// Decode a sample as UTF-8, or as Windows-1252 when it is not, which is
/// what Latin-1 files from older systems are in practice. `None` when the
/// bytes do not look like text at all.
pub fn decode(bytes: &[u8]) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        // A multibyte sequence cut by the end of the sample is still UTF-8.
        Err(err) if err.error_len().is_none() => {
            Cow::Borrowed(std::str::from_utf8(&bytes[..err.valid_up_to()]).expect("valid prefix"))
        }
        Err(_) => Cow::Owned(bytes.iter().map(|b| windows_1252(*b)).collect()),
    };
    let total = text.chars().count();
    let control = text
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
        .count();
    if control as f32 > total as f32 * MAX_CONTROL_RATIO {
        return None;
    }
    Some(text.into_owned())
}

/// Windows-1252 is Latin-1 with printable characters in the C1 range.
fn windows_1252(byte: u8) -> char {
    const C1: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match byte {
        0x80..=0x9f => C1[usize::from(byte - 0x80)],
        other => char::from(other),
    }
}

/// Bidirectional formatting characters. `char::is_control` covers only the
/// C0 and C1 controls, and these can make `invoice\u{202E}fdp.exe` display
/// as `invoiceexe.pdf` in a download prompt, a URL bar or a thumbnail.
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// The first `ROWS` lines, tabs expanded, cut at `COLUMNS`, with control
/// and invisible formatting characters dropped.
pub fn lines(text: &str) -> Vec<String> {
    text.trim_start_matches('\u{FEFF}')
        .lines()
        .take(ROWS)
        .map(|line| {
            let mut out = String::new();
            let mut column = 0;
            for c in line.chars() {
                if column >= COLUMNS {
                    break;
                }
                match c {
                    '\t' => {
                        for _ in 0..(4 - column % 4) {
                            if column < COLUMNS {
                                out.push(' ');
                                column += 1;
                            }
                        }
                    }
                    c if c.is_control() || is_bidi_control(c) => {}
                    c => {
                        out.push(c);
                        column += 1;
                    }
                }
            }
            out
        })
        .collect()
}

/// A `width` by `height` PGM of the sample on a page, or `None` when the
/// bytes are not text.
pub fn render(bytes: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let text = decode(bytes)?;
    let lines = lines(&text);
    let font = FontRef::try_from_slice(FONT).expect("the embedded font is valid");

    let (w, h) = (width as usize, height as usize);
    let mut canvas = vec![BACKGROUND; w * h];
    // The margin follows the shorter edge, so a box of any shape keeps a
    // page inside it.
    let margin = w.min(h) / 16;
    let page_height = h - 2 * margin;
    let page_width = ((page_height as f32 * PAGE_RATIO) as usize).min(w - 2 * margin);
    let page_left = (w - page_width) / 2;
    for y in margin..margin + page_height {
        canvas[y * w + page_left..y * w + page_left + page_width].fill(PAGE);
    }

    // The font size follows from the column count and the glyph advance.
    let pad = page_width as f32 * 0.08;
    let text_width = page_width as f32 - 2.0 * pad;
    let advance_per_px = font
        .as_scaled(PxScale::from(100.0))
        .h_advance(font.glyph_id('M'))
        / 100.0;
    let px = text_width / (COLUMNS as f32 * advance_per_px);
    let scaled = font.as_scaled(PxScale::from(px));
    let advance = scaled.h_advance(font.glyph_id('M'));
    let line_height = scaled.height() * 1.25;
    let left = page_left as f32 + pad;
    let bottom = (margin + page_height) as f32 - pad;

    for (row, line) in lines.iter().enumerate() {
        let baseline = margin as f32 + pad + scaled.ascent() + row as f32 * line_height;
        if baseline - scaled.descent() > bottom {
            break;
        }
        for (column, c) in line.chars().enumerate() {
            let glyph = font
                .glyph_id(c)
                .with_scale_and_position(px, point(left + column as f32 * advance, baseline));
            let Some(outlined) = font.outline_glyph(glyph) else {
                continue;
            };
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let x = bounds.min.x as i32 + gx as i32;
                let y = bounds.min.y as i32 + gy as i32;
                if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                    return;
                }
                let pixel = &mut canvas[y as usize * w + x as usize];
                let c = coverage.clamp(0.0, 1.0);
                *pixel = (f32::from(*pixel) * (1.0 - c) + f32::from(INK) * c) as u8;
            });
        }
    }

    let mut out = format!("P5\n{width} {height}\n255\n").into_bytes();
    out.extend_from_slice(&canvas);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf8_and_windows_1252() {
        assert_eq!(decode("héllo\n".as_bytes()).unwrap(), "héllo\n");
        assert_eq!(decode(b"caf\xe9 \x80 \x9c\n").unwrap(), "café € œ\n");
        // A sample cut inside a multibyte character.
        let cut = &"été".as_bytes()[..4];
        assert_eq!(decode(cut).unwrap(), "ét");
        assert_eq!(decode(b"").unwrap(), "");
    }

    #[test]
    fn binary_is_not_text() {
        assert_eq!(decode(b"text\0more"), None);
        let mostly_control: Vec<u8> = (1u8..32).cycle().take(200).collect();
        assert_eq!(decode(&mostly_control), None);
        let bell = b"a line\x07\nand plenty more harmless text to dilute it\n";
        assert!(decode(bell).is_some());
    }

    #[test]
    fn lays_out_lines() {
        let text = "\u{FEFF}a\tb\r\n\u{202E}hidden\x1b[0m\n".to_string()
            + &"x".repeat(100)
            + "\n"
            + &"l\n".repeat(30);
        let lines = lines(&text);
        assert_eq!(lines.len(), ROWS);
        assert_eq!(lines[0], "a   b");
        assert_eq!(lines[1], "hidden[0m");
        assert_eq!(lines[2], "x".repeat(COLUMNS));
        assert_eq!(lines[3], "l");
    }

    #[test]
    fn renders_a_page_with_ink() {
        let pgm = render(b"Hello, agrafe\nsecond line\n", 512, 512).unwrap();
        let header = b"P5\n512 512\n255\n";
        assert_eq!(&pgm[..header.len()], header);
        let pixels = &pgm[header.len()..];
        assert_eq!(pixels.len(), 512 * 512);
        assert!(pixels.contains(&BACKGROUND));
        assert!(pixels.contains(&PAGE));
        assert!(pixels.iter().any(|p| *p < 0x80), "some ink");
        // An empty file is a blank page, still a text file.
        let blank = render(b"", 64, 64).unwrap();
        assert!(!blank[b"P5\n64 64\n255\n".len()..].iter().any(|p| *p < 0x80));
        assert_eq!(render(b"\0\0\0", 64, 64), None);
        // Boxes of any shape, including degenerate ones.
        for (width, height) in [(16, 512), (512, 16), (1, 1), (0, 0)] {
            assert!(render(b"x\n", width, height).is_some(), "{width}x{height}");
        }
    }
}
