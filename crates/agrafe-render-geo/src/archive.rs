//! The ZIP archives a map comes in, a KMZ or a zipped shapefile: opened
//! only when their entries are few enough to be a map and what goes with
//! it, and each entry read with a cap on what it inflates to, since the
//! size its header claims is the writer's.

use std::io::{Cursor, Read};

use zip::ZipArchive;

/// Archives with more entries than this are not a map and its icons or
/// its layers.
pub(crate) const MAX_ENTRIES: usize = 10_000;
/// What an archive is read to at most, inflated, entry by entry or in
/// all: the size a bare document may be, and well under the sandbox's
/// memory.
pub(crate) const MAX_INFLATED: u64 = 64 * 1024 * 1024;

/// The ZIP archive `bytes`, when it is one with at most `MAX_ENTRIES`.
pub(crate) fn open(bytes: &[u8]) -> Option<ZipArchive<Cursor<&[u8]>>> {
    let zip = ZipArchive::new(Cursor::new(bytes)).ok()?;
    (zip.len() <= MAX_ENTRIES).then_some(zip)
}

/// The index and lowercased name of each entry of `zip`, in archive
/// order, skipping the `__MACOSX/` sidecars a Mac zips in and the names
/// that are not UTF-8.
pub(crate) fn names<'a>(
    zip: &'a ZipArchive<Cursor<&[u8]>>,
) -> impl Iterator<Item = (usize, String)> + 'a {
    (0..zip.len()).filter_map(|index| {
        let name = zip.name_for_index(index)?.ok()?.to_ascii_lowercase();
        (!name.starts_with("__macosx/")).then_some((index, name))
    })
}

/// The entry `index` of `zip`, inflated, when it is at most `max` bytes.
pub(crate) fn read(zip: &mut ZipArchive<Cursor<&[u8]>>, index: usize, max: u64) -> Option<Vec<u8>> {
    let entry = zip.by_index(index).ok()?;
    let mut data = Vec::new();
    entry.take(max + 1).read_to_end(&mut data).ok()?;
    (data.len() as u64 <= max).then_some(data)
}

/// A ZIP of `entries`, stored, in that order.
#[cfg(test)]
pub(crate) fn zipped(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
