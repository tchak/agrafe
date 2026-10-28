//! Where a blob's objects live in the bucket: `<shard>/<blob id>/<name>`,
//! the shard being the last two hex digits of the id. See DESIGN.md,
//! "Object keys".
//!
//! The id is a UUID v7, ordered by time, and a key that began with it
//! would put every new object at the end of the bucket's key space. The
//! stores that split a bucket into key ranges as it grows (OpenIO, behind
//! OVHcloud's S3, splits at the median key; S3 did the same before 2018)
//! would then send every write to the newest range, which is their
//! documented worst case. The shard, two hex digits taken from the random
//! low bits of the id, spreads new keys over 256 ranges. It is a function
//! of the id, so nothing is stored to find an object, and the database can
//! compute it (`get_byte(uuid_send(id), 15)`) to walk the rows in key
//! order, which the audit relies on.
//!
//! Every key is built here, and the audit parses them here, so the layout
//! has one definition.

use object_store::path::Path;
use uuid::Uuid;

use super::Representation;

/// The shard of a blob: the low byte of its id, the last two hex digits
/// of its text form.
pub fn shard(id: Uuid) -> u8 {
    id.as_bytes()[15]
}

/// Where a blob's objects are, `<shard>/<blob id>`, with no trailing
/// slash. Everything under it is the blob's, and a purge deletes it all.
pub fn prefix(id: Uuid) -> String {
    format!("{:02x}/{id}", shard(id))
}

/// The original, `<shard>/<blob id>/original`. Blobs are write-once, so
/// there is one.
pub fn original(id: Uuid) -> String {
    format!("{}/original", prefix(id))
}

/// A representation, `<shard>/<blob id>/thumbnail` or `.../preview`.
pub fn representation(id: Uuid, kind: Representation) -> String {
    format!("{}/{kind}", prefix(id))
}

/// Source part `n` of a generated blob, `<shard>/<blob id>/source/<n>`.
pub fn source(id: Uuid, n: u32) -> String {
    format!("{}/source/{n}", prefix(id))
}

/// The associated data of the object at `path`: its name within the
/// blob, `<blob id>/<name>`, which is the path without its shard. The
/// AEAD binds a ciphertext to it, so one copied under another blob or
/// another name does not decrypt; the shard is a function of the id and
/// adds nothing to the binding, and leaving it out keeps a copy of the
/// bucket to another layout a copy, not a re-encryption.
pub fn associated_data(path: &str) -> &str {
    path.split_once('/').map_or(path, |(_, name)| name)
}

/// Where a blob sorts among the bucket's keys: the stores list keys in
/// byte order, which for this layout is the shard, then the id, since
/// the hyphenated lowercase UUID sorts as the `uuid` column does. The
/// audit walks the rows in this order to merge them with the listing.
pub fn order(id: Uuid) -> (u8, Uuid) {
    (shard(id), id)
}

/// The blob an object belongs to, when its key is in the layout: the
/// shard, the id as agrafe writes it, hyphenated and lowercase, under
/// the shard that is its own, and a name after it. Any other key is
/// foreign, not agrafe's; another spelling of a UUID the parser would
/// take is not, since the audit matches names under the canonical prefix
/// and merges the listing in key order.
pub fn owner(path: &Path) -> Option<Uuid> {
    let mut parts = path.parts();
    let shard_part = parts.next()?;
    let id_part = parts.next()?;
    parts.next()?;
    let shard_part = shard_part.as_ref();
    let id_part = id_part.as_ref();
    let hex = |s: &str| s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if shard_part.len() != 2 || !hex(shard_part) {
        return None;
    }
    let canonical = id_part.len() == 36
        && id_part
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-'));
    if !canonical {
        return None;
    }
    let id: Uuid = id_part.parse().ok()?;
    (format!("{:02x}", shard(id)) == shard_part).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shard_is_the_last_two_hex_digits_of_the_id() {
        let id: Uuid = "01999e6c-0000-7000-8000-0000000000ab".parse().unwrap();
        assert_eq!(shard(id), 0xab);
        assert_eq!(prefix(id), format!("ab/{id}"));
        assert_eq!(original(id), format!("ab/{id}/original"));
        assert_eq!(
            representation(id, Representation::Thumbnail),
            format!("ab/{id}/thumbnail")
        );
        assert_eq!(source(id, 3), format!("ab/{id}/source/3"));
        let low: Uuid = "01999e6c-0000-7000-8000-000000000005".parse().unwrap();
        assert_eq!(prefix(low), format!("05/{low}"), "zero padded");
    }

    #[test]
    fn the_associated_data_is_the_key_without_its_shard() {
        let id: Uuid = "01999e6c-0000-7000-8000-0000000000ab".parse().unwrap();
        assert_eq!(associated_data(&original(id)), format!("{id}/original"));
        assert_eq!(
            associated_data(&representation(id, Representation::Preview)),
            format!("{id}/preview")
        );
        assert_eq!(associated_data(&source(id, 3)), format!("{id}/source/3"));
        assert_eq!(
            associated_data("README"),
            "README",
            "a key with no shard is itself"
        );
    }

    #[test]
    fn the_key_order_is_the_shard_then_the_id() {
        let early: Uuid = "01999e6c-0000-7000-8000-0000000000ff".parse().unwrap();
        let late: Uuid = "01999e6d-0000-7000-8000-000000000000".parse().unwrap();
        assert!(early < late, "by time");
        assert!(order(late) < order(early), "by key");
        assert_eq!(
            Path::from(original(late)) < Path::from(original(early)),
            order(late) < order(early)
        );
    }

    #[test]
    fn an_owner_is_a_shard_and_a_uuid_with_a_name_after_them() {
        // A fixed id with letters in its shard and its body: the spelling
        // checks below uppercase them, which a random id may have none of.
        let id: Uuid = "01999e6c-0000-7000-8000-0000000000ab".parse().unwrap();
        let p = |key: String| Path::from(key);
        assert_eq!(owner(&p(original(id))), Some(id));
        assert_eq!(owner(&p(source(id, 3))), Some(id));
        assert_eq!(owner(&p(prefix(id))), None, "no name");
        assert_eq!(owner(&p(format!("{id}/original"))), None, "no shard");
        assert_eq!(owner(&p("junk/original".into())), None);
        assert_eq!(owner(&p("ab/junk/original".into())), None);
        let other = format!("{:02x}", shard(id).wrapping_add(1));
        assert_eq!(
            owner(&p(format!("{other}/{id}/original"))),
            None,
            "under another shard"
        );
        assert_eq!(
            owner(&p(format!("{:02X}/{id}/original", shard(id)))),
            None,
            "the shard is lowercase"
        );
        assert_eq!(
            owner(&p(format!("{:02x}/{}/original", shard(id), id.as_simple()))),
            None,
            "not the spelling agrafe writes"
        );
        assert_eq!(
            owner(&p(format!(
                "{:02x}/{}/original",
                shard(id),
                id.to_string().to_uppercase()
            ))),
            None
        );
        assert_eq!(owner(&p("README".into())), None);
    }
}
