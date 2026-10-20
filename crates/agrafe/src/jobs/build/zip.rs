//! A ZIP writer that never seeks: each entry is a local header, the bytes,
//! then a data descriptor with the CRC and sizes, and the central directory
//! comes last. Zip64 is decided per entry from its size before its header
//! goes out, since a streamed header cannot be rewritten; the central
//! directory switches to zip64 on its own for the entry count and the
//! offsets. See [DESIGN.md, "Archives"].
//!
//! The writer produces bytes and does no I/O: [`ZipWriter::begin_entry`]
//! and [`ZipWriter::end_entry`] return the headers to emit, the caller
//! emits the member's bytes itself and reports them with
//! [`ZipWriter::data`], and [`ZipWriter::finish`] returns the directory.
//!
//! Three kinds of entry: a stored one of known size (an archive member), a
//! deflated one of unknown size (a spreadsheet's XML, compressed as it is
//! produced, whose compressed bytes the caller reports with
//! [`ZipWriter::compressed`]), and a whole stored one given at once
//! ([`ZipWriter::whole_entry`]), which carries its CRC in the local header
//! and needs no descriptor: what an ODS `mimetype` entry has to be. A
//! deflated entry's size is not known when its header goes out, so it
//! always takes zip64 fields, which the format allows at any size: the
//! header announces `0xFFFFFFFF` and a zip64 extra of zeros, and the
//! descriptor carries the real sizes as 64-bit values.
//!
//! [DESIGN.md, "Archives"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#archives

use jiff::Timestamp;
use jiff::tz::TimeZone;

const LOCAL_HEADER: u32 = 0x0403_4b50;
const DATA_DESCRIPTOR: u32 = 0x0807_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;
const ZIP64_END_OF_CENTRAL_DIRECTORY: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_EXTRA: u16 = 0x0001;

/// Bit 3: sizes and CRC follow the data. Bit 11: names are UTF-8.
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;
const VERSION_STORED: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// Made on Unix, so the external attributes carry a mode.
const MADE_BY: u16 = (3 << 8) | VERSION_ZIP64;
/// `-rw-r--r--`, a regular file.
const EXTERNAL_ATTRIBUTES: u32 = 0o100_644 << 16;

/// A 32-bit field holds less than this; at or above, the value goes to a
/// zip64 field.
const FIELD_LIMIT_32: u64 = 0xFFFF_FFFF;
const FIELD_LIMIT_16: u64 = 0xFFFF;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZipError {
    #[error("entry name `{0}` is longer than 65535 bytes")]
    NameTooLong(String),
    #[error("entry `{name}` was declared {declared} bytes and received {written}")]
    SizeMismatch {
        name: String,
        declared: u64,
        written: u64,
    },
    #[error("an entry is still open")]
    EntryOpen,
    #[error("no entry is open")]
    NoEntry,
    #[error("the open entry is not deflated")]
    NotDeflated,
}

/// How an entry's bytes are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Stored = 0,
    Deflated = 8,
}

/// MS-DOS date and time, what a ZIP entry carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DosTime {
    date: u16,
    time: u16,
}

impl DosTime {
    /// `at` in UTC, to two-second precision, clamped to the format's
    /// 1980–2107 range.
    pub fn from_timestamp(at: Timestamp) -> Self {
        let civil = at.to_zoned(TimeZone::UTC).datetime();
        let year = civil.year().clamp(1980, 2107) as u16;
        let (month, day, hour, minute, second) = if civil.year() < 1980 {
            (1, 1, 0, 0, 0)
        } else if civil.year() > 2107 {
            (12, 31, 23, 59, 58)
        } else {
            (
                civil.month() as u16,
                civil.day() as u16,
                civil.hour() as u16,
                civil.minute() as u16,
                civil.second() as u16,
            )
        };
        Self {
            date: ((year - 1980) << 9) | (month << 5) | day,
            time: (hour << 11) | (minute << 5) | (second / 2),
        }
    }
}

struct Open {
    name: Vec<u8>,
    offset: u64,
    method: Method,
    /// The size the local header announced, for a stored entry.
    declared: Option<u64>,
    zip64: bool,
    time: DosTime,
    crc: crc32fast::Hasher,
    /// Plaintext bytes reported so far.
    written: u64,
    /// Bytes emitted after the header so far: the plaintext of a stored
    /// entry, what the caller reported for a deflated one.
    compressed: u64,
}

struct Central {
    name: Vec<u8>,
    offset: u64,
    method: Method,
    /// Whether the entry has a data descriptor, which its flags say.
    descriptor: bool,
    size: u64,
    compressed: u64,
    crc: u32,
    zip64: bool,
    time: DosTime,
}

pub struct ZipWriter {
    /// Bytes emitted so far, headers and data alike.
    offset: u64,
    entries: Vec<Central>,
    open: Option<Open>,
    /// Sizes and offsets at or above this use zip64 fields. The format's
    /// limit, lowered by tests to exercise the zip64 paths on small files.
    threshold: u64,
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipWriter {
    pub fn new() -> Self {
        Self::with_threshold(FIELD_LIMIT_32)
    }

    fn with_threshold(threshold: u64) -> Self {
        Self {
            offset: 0,
            entries: Vec::new(),
            open: None,
            threshold,
        }
    }

    /// Open a stored entry of `size` bytes named `name`: the local header
    /// to emit. The bytes follow, reported through [`Self::data`].
    pub fn begin_entry(
        &mut self,
        name: &str,
        size: u64,
        time: DosTime,
    ) -> Result<Vec<u8>, ZipError> {
        let zip64 = size >= self.threshold;
        self.open_entry(name, time, Method::Stored, Some(size), zip64, None)
    }

    /// Open a deflated entry of a size not known yet: the local header to
    /// emit, with zip64 fields since the size may turn out to need them.
    /// The caller compresses the bytes itself, reporting the plain ones
    /// through [`Self::data`] and the compressed ones it emits through
    /// [`Self::compressed`].
    pub fn begin_deflated(&mut self, name: &str, time: DosTime) -> Result<Vec<u8>, ZipError> {
        self.open_entry(name, time, Method::Deflated, None, true, None)
    }

    /// A stored entry given whole: the local header, carrying the CRC, and
    /// the bytes, with no descriptor to follow. The entry is closed.
    pub fn whole_entry(
        &mut self,
        name: &str,
        bytes: &[u8],
        time: DosTime,
    ) -> Result<Vec<u8>, ZipError> {
        let size = bytes.len() as u64;
        let crc = crc32fast::hash(bytes);
        let zip64 = size >= self.threshold;
        let mut out = self.open_entry(name, time, Method::Stored, Some(size), zip64, Some(crc))?;
        out.extend_from_slice(bytes);
        let open = self.open.take().expect("just opened");
        self.offset += size;
        self.entries.push(Central {
            name: open.name,
            offset: open.offset,
            method: Method::Stored,
            descriptor: false,
            size,
            compressed: size,
            crc,
            zip64,
            time,
        });
        Ok(out)
    }

    /// The local header of an entry, which `crc` turns into one without a
    /// descriptor; `declared` is the size a stored entry announces.
    fn open_entry(
        &mut self,
        name: &str,
        time: DosTime,
        method: Method,
        declared: Option<u64>,
        zip64: bool,
        crc: Option<u32>,
    ) -> Result<Vec<u8>, ZipError> {
        if self.open.is_some() {
            return Err(ZipError::EntryOpen);
        }
        if name.len() > FIELD_LIMIT_16 as usize {
            return Err(ZipError::NameTooLong(name.to_string()));
        }
        let mut out = Vec::with_capacity(30 + name.len() + 20);
        put_u32(&mut out, LOCAL_HEADER);
        put_u16(&mut out, if zip64 { VERSION_ZIP64 } else { VERSION_STORED });
        put_u16(&mut out, flags(crc.is_none()));
        put_u16(&mut out, method as u16);
        put_u16(&mut out, time.time);
        put_u16(&mut out, time.date);
        put_u32(&mut out, crc.unwrap_or(0));
        // A stored entry's sizes are known up front, so they go in the
        // header too for readers that walk the entries without the
        // directory; the descriptor repeats them with the CRC. A deflated
        // entry's are zero until its descriptor, in the zip64 extra too.
        let size = declared.unwrap_or(0);
        let field = if zip64 { FIELD_LIMIT_32 } else { size } as u32;
        put_u32(&mut out, field);
        put_u32(&mut out, field);
        put_u16(&mut out, name.len() as u16);
        put_u16(&mut out, if zip64 { 20 } else { 0 });
        out.extend_from_slice(name.as_bytes());
        if zip64 {
            put_u16(&mut out, ZIP64_EXTRA);
            put_u16(&mut out, 16);
            put_u64(&mut out, size);
            put_u64(&mut out, size);
        }
        self.open = Some(Open {
            name: name.as_bytes().to_vec(),
            offset: self.offset,
            method,
            declared,
            zip64,
            time,
            crc: crc32fast::Hasher::new(),
            written: 0,
            compressed: 0,
        });
        self.offset += out.len() as u64;
        Ok(out)
    }

    /// Plain bytes of the open entry: emitted as they are for a stored
    /// entry, handed to the compressor for a deflated one.
    pub fn data(&mut self, bytes: &[u8]) -> Result<(), ZipError> {
        let open = self.open.as_mut().ok_or(ZipError::NoEntry)?;
        open.crc.update(bytes);
        open.written += bytes.len() as u64;
        if open.method == Method::Stored {
            open.compressed += bytes.len() as u64;
            self.offset += bytes.len() as u64;
        }
        Ok(())
    }

    /// `n` compressed bytes of the open deflated entry the caller has
    /// emitted.
    pub fn compressed(&mut self, n: u64) -> Result<(), ZipError> {
        let open = self.open.as_mut().ok_or(ZipError::NoEntry)?;
        if open.method != Method::Deflated {
            return Err(ZipError::NotDeflated);
        }
        open.compressed += n;
        self.offset += n;
        Ok(())
    }

    /// Close the open entry: the data descriptor to emit.
    pub fn end_entry(&mut self) -> Result<Vec<u8>, ZipError> {
        let open = self.open.take().ok_or(ZipError::NoEntry)?;
        if let Some(declared) = open.declared
            && open.written != declared
        {
            return Err(ZipError::SizeMismatch {
                name: String::from_utf8_lossy(&open.name).into_owned(),
                declared,
                written: open.written,
            });
        }
        let crc = open.crc.finalize();
        let mut out = Vec::with_capacity(24);
        put_u32(&mut out, DATA_DESCRIPTOR);
        put_u32(&mut out, crc);
        if open.zip64 {
            put_u64(&mut out, open.compressed);
            put_u64(&mut out, open.written);
        } else {
            put_u32(&mut out, open.compressed as u32);
            put_u32(&mut out, open.written as u32);
        }
        self.offset += out.len() as u64;
        self.entries.push(Central {
            name: open.name,
            offset: open.offset,
            method: open.method,
            descriptor: true,
            size: open.written,
            compressed: open.compressed,
            crc,
            zip64: open.zip64,
            time: open.time,
        });
        Ok(out)
    }

    /// The central directory and the end records: the tail of the file.
    pub fn finish(self) -> Result<Vec<u8>, ZipError> {
        if self.open.is_some() {
            return Err(ZipError::EntryOpen);
        }
        let directory_offset = self.offset;
        let mut out = Vec::new();
        let mut any_zip64 = false;
        for entry in &self.entries {
            let size_overflows =
                entry.zip64 || entry.size >= self.threshold || entry.compressed >= self.threshold;
            let offset_overflows = entry.offset >= self.threshold;
            any_zip64 |= size_overflows || offset_overflows;
            let mut extra = Vec::new();
            if size_overflows {
                put_u64(&mut extra, entry.size);
                put_u64(&mut extra, entry.compressed);
            }
            if offset_overflows {
                put_u64(&mut extra, entry.offset);
            }
            put_u32(&mut out, CENTRAL_HEADER);
            put_u16(&mut out, MADE_BY);
            put_u16(
                &mut out,
                if extra.is_empty() {
                    VERSION_STORED
                } else {
                    VERSION_ZIP64
                },
            );
            put_u16(&mut out, flags(entry.descriptor));
            put_u16(&mut out, entry.method as u16);
            put_u16(&mut out, entry.time.time);
            put_u16(&mut out, entry.time.date);
            put_u32(&mut out, entry.crc);
            let (compressed, size) = if size_overflows {
                (FIELD_LIMIT_32, FIELD_LIMIT_32)
            } else {
                (entry.compressed, entry.size)
            };
            put_u32(&mut out, compressed as u32);
            put_u32(&mut out, size as u32);
            put_u16(&mut out, entry.name.len() as u16);
            put_u16(
                &mut out,
                if extra.is_empty() {
                    0
                } else {
                    4 + extra.len() as u16
                },
            );
            put_u16(&mut out, 0); // comment
            put_u16(&mut out, 0); // disk
            put_u16(&mut out, 0); // internal attributes
            put_u32(&mut out, EXTERNAL_ATTRIBUTES);
            put_u32(
                &mut out,
                if offset_overflows {
                    FIELD_LIMIT_32
                } else {
                    entry.offset
                } as u32,
            );
            out.extend_from_slice(&entry.name);
            if !extra.is_empty() {
                put_u16(&mut out, ZIP64_EXTRA);
                put_u16(&mut out, extra.len() as u16);
                out.extend_from_slice(&extra);
            }
        }
        let directory_size = out.len() as u64;
        let count = self.entries.len() as u64;
        let needs_zip64_end = any_zip64
            || count >= FIELD_LIMIT_16
            || directory_size >= self.threshold
            || directory_offset >= self.threshold;
        if needs_zip64_end {
            let zip64_end_offset = directory_offset + directory_size;
            put_u32(&mut out, ZIP64_END_OF_CENTRAL_DIRECTORY);
            put_u64(&mut out, 44); // size of the rest of this record
            put_u16(&mut out, MADE_BY);
            put_u16(&mut out, VERSION_ZIP64);
            put_u32(&mut out, 0); // this disk
            put_u32(&mut out, 0); // disk with the directory
            put_u64(&mut out, count);
            put_u64(&mut out, count);
            put_u64(&mut out, directory_size);
            put_u64(&mut out, directory_offset);
            put_u32(&mut out, ZIP64_LOCATOR);
            put_u32(&mut out, 0); // disk with the zip64 end record
            put_u64(&mut out, zip64_end_offset);
            put_u32(&mut out, 1); // disks in total
        }
        put_u32(&mut out, END_OF_CENTRAL_DIRECTORY);
        put_u16(&mut out, 0); // this disk
        put_u16(&mut out, 0); // disk with the directory
        let count_field = count.min(FIELD_LIMIT_16) as u16;
        put_u16(&mut out, count_field);
        put_u16(&mut out, count_field);
        put_u32(&mut out, directory_size.min(FIELD_LIMIT_32) as u32);
        put_u32(&mut out, directory_offset.min(FIELD_LIMIT_32) as u32);
        put_u16(&mut out, 0); // comment
        Ok(out)
    }
}

fn flags(descriptor: bool) -> u16 {
    if descriptor {
        FLAG_UTF8 | FLAG_DESCRIPTOR
    } else {
        FLAG_UTF8
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};

    use super::*;

    fn at() -> Timestamp {
        "2026-10-16T08:44:26Z".parse().unwrap()
    }

    /// Write `entries` through a writer with `threshold`, concatenating
    /// headers and data as the build does.
    fn archive(threshold: u64, entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::with_threshold(threshold);
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend(
                zip.begin_entry(name, data.len() as u64, DosTime::from_timestamp(at()))
                    .unwrap(),
            );
            // In two pieces, as segments arrive.
            let (a, b) = data.split_at(data.len() / 2);
            zip.data(a).unwrap();
            zip.data(b).unwrap();
            out.extend_from_slice(data);
            out.extend(zip.end_entry().unwrap());
        }
        out.extend(zip.finish().unwrap());
        out
    }

    /// Every entry as the `zip` crate reads it back: name and bytes, with
    /// the method checked against `stored`.
    fn read_back(bytes: &[u8], stored: bool) -> Vec<(String, Vec<u8>)> {
        let mut archive = ::zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|i| {
                let mut file = archive.by_index(i).unwrap();
                let want = if stored {
                    ::zip::CompressionMethod::Stored
                } else {
                    ::zip::CompressionMethod::Deflated
                };
                assert_eq!(file.compression(), want);
                let mut data = Vec::new();
                // Checks the CRC against the directory.
                file.read_to_end(&mut data).unwrap();
                assert_eq!(file.size(), data.len() as u64);
                assert_eq!(file.unix_mode(), Some(0o100_644));
                let modified = file.last_modified().unwrap();
                assert_eq!(
                    (
                        modified.year(),
                        modified.month(),
                        modified.day(),
                        modified.hour(),
                        modified.minute(),
                        modified.second()
                    ),
                    (2026, 10, 16, 8, 44, 26)
                );
                (file.name().unwrap().into_owned(), data)
            })
            .collect()
    }

    #[test]
    fn round_trips_stored_entries() {
        let big: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let entries: &[(&str, &[u8])] = &[
            ("docs/été.pdf", b"hello"),
            ("empty", b""),
            ("big.bin", &big),
        ];
        let bytes = archive(FIELD_LIMIT_32, entries);
        let got = read_back(&bytes, true);
        assert_eq!(got.len(), 3);
        for ((name, data), (want_name, want_data)) in got.iter().zip(entries) {
            assert_eq!(name, want_name);
            assert_eq!(data, want_data);
        }
        // No zip64 anywhere on a small file.
        assert!(
            !bytes
                .windows(4)
                .any(|w| w == ZIP64_END_OF_CENTRAL_DIRECTORY.to_le_bytes())
        );
    }

    #[test]
    fn empty_archive() {
        let bytes = archive(FIELD_LIMIT_32, &[]);
        assert_eq!(bytes.len(), 22);
        assert!(read_back(&bytes, true).is_empty());
    }

    /// With the threshold lowered to a few bytes, every size and offset
    /// takes the zip64 path, and the file still reads back.
    #[test]
    fn zip64_entries_and_directory() {
        let entries: &[(&str, &[u8])] = &[
            ("a.txt", b"0123456789"),
            ("b.txt", b"x"),
            ("c.txt", b"0123456789abcdef0123456789abcdef"),
        ];
        let bytes = archive(16, entries);
        assert!(
            bytes
                .windows(4)
                .any(|w| w == ZIP64_END_OF_CENTRAL_DIRECTORY.to_le_bytes())
        );
        let got = read_back(&bytes, true);
        assert_eq!(got.len(), 3);
        for ((name, data), (want_name, want_data)) in got.iter().zip(entries) {
            assert_eq!(name, want_name);
            assert_eq!(data, want_data);
        }
    }

    /// Past 65 535 entries the counts move to the zip64 end record.
    #[test]
    fn many_entries() {
        let mut zip = ZipWriter::new();
        let mut out = Vec::new();
        let n = 0x1_0001;
        for i in 0..n {
            out.extend(
                zip.begin_entry(&format!("{i}"), 1, DosTime::from_timestamp(at()))
                    .unwrap(),
            );
            zip.data(b"x").unwrap();
            out.push(b'x');
            out.extend(zip.end_entry().unwrap());
        }
        out.extend(zip.finish().unwrap());
        let archive = ::zip::ZipArchive::new(Cursor::new(out)).unwrap();
        assert_eq!(archive.len(), n);
    }

    /// Deflated entries compressed by the caller in pieces, a whole stored
    /// one first as an ODS has it: every entry reads back, the whole one
    /// with its CRC in the local header and no descriptor flag, the
    /// deflated one with zip64 fields in its header and descriptor.
    #[test]
    fn deflated_and_whole_entries() {
        let t = DosTime::from_timestamp(at());
        let mut zip = ZipWriter::new();
        let mut out = Vec::new();
        out.extend(zip.whole_entry("mimetype", b"text/plain", t).unwrap());
        // Local header: flags at 6, CRC at 14, then the name at 30.
        assert_eq!(&out[6..8], &FLAG_UTF8.to_le_bytes());
        assert_eq!(&out[14..18], &crc32fast::hash(b"text/plain").to_le_bytes());
        assert_eq!(&out[30..38], b"mimetype");
        assert_eq!(&out[38..], b"text/plain");

        let text: Vec<u8> = (0..20_000u32)
            .flat_map(|i| format!("<row r=\"{i}\"/>").into_bytes())
            .collect();
        let header = zip.begin_deflated("content.xml", t).unwrap();
        // Version 45 at 4, sizes 0xFFFFFFFF at 18 and 22, the name, then
        // the zip64 extra: id 1, 16 bytes, two zero sizes.
        assert_eq!(&header[4..6], &VERSION_ZIP64.to_le_bytes());
        assert_eq!(&header[18..26], &[0xFF; 8]);
        assert_eq!(&header[30..41], b"content.xml");
        assert_eq!(&header[41..45], &[1, 0, 16, 0]);
        assert_eq!(&header[45..], &[0; 16]);
        out.extend(header);
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        for piece in text.chunks(7000) {
            zip.data(piece).unwrap();
            encoder.write_all(piece).unwrap();
            let compressed = std::mem::take(encoder.get_mut());
            zip.compressed(compressed.len() as u64).unwrap();
            out.extend(compressed);
        }
        let compressed = encoder.finish().unwrap();
        zip.compressed(compressed.len() as u64).unwrap();
        out.extend(compressed);
        let descriptor = zip.end_entry().unwrap();
        assert_eq!(descriptor.len(), 24, "64-bit sizes");
        assert_eq!(&descriptor[16..24], &(text.len() as u64).to_le_bytes());
        out.extend(descriptor);
        out.extend(zip.finish().unwrap());
        assert!(out.len() < text.len() / 4, "{} bytes", out.len());

        let mut archive = ::zip::ZipArchive::new(Cursor::new(&out)).unwrap();
        assert_eq!(archive.len(), 2);
        let mut first = archive.by_index(0).unwrap();
        assert_eq!(first.name().unwrap(), "mimetype");
        assert_eq!(first.compression(), ::zip::CompressionMethod::Stored);
        let mut got = Vec::new();
        first.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"text/plain");
        drop(first);
        let mut second = archive.by_index(1).unwrap();
        assert_eq!(second.name().unwrap(), "content.xml");
        assert_eq!(second.compression(), ::zip::CompressionMethod::Deflated);
        let mut got = Vec::new();
        second.read_to_end(&mut got).unwrap();
        assert_eq!(got, text);
    }

    #[test]
    fn misuse() {
        let mut zip = ZipWriter::new();
        assert_eq!(zip.data(b"x"), Err(ZipError::NoEntry));
        assert_eq!(zip.end_entry(), Err(ZipError::NoEntry));
        assert_eq!(zip.compressed(1), Err(ZipError::NoEntry));
        let t = DosTime::from_timestamp(at());
        zip.begin_entry("a", 2, t).unwrap();
        assert_eq!(zip.begin_entry("b", 2, t), Err(ZipError::EntryOpen));
        assert_eq!(zip.whole_entry("b", b"", t), Err(ZipError::EntryOpen));
        assert_eq!(zip.compressed(1), Err(ZipError::NotDeflated));
        zip.data(b"x").unwrap();
        assert!(matches!(
            zip.end_entry(),
            Err(ZipError::SizeMismatch { .. })
        ));
        let long = "n".repeat(65_536);
        assert!(matches!(
            ZipWriter::new().begin_entry(&long, 0, t),
            Err(ZipError::NameTooLong(_))
        ));
        // A deflated entry has no size ceiling of its own.
        let mut zip = ZipWriter::new();
        zip.begin_deflated("d", t).unwrap();
        assert_eq!(zip.compressed(FIELD_LIMIT_32 + 1), Ok(()));
    }

    #[test]
    fn dos_time_clamps() {
        let t = DosTime::from_timestamp("1970-01-01T00:00:00Z".parse().unwrap());
        assert_eq!((t.date, t.time), ((1 << 5) | 1, 0));
        let t = DosTime::from_timestamp("2200-06-01T12:00:00Z".parse().unwrap());
        assert_eq!(t.date >> 9, 2107 - 1980);
    }
}
