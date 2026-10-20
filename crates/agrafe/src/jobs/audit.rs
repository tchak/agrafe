//! The reconciliation sweep: the bucket listing against the `blobs` table,
//! both read whole, so a bucket or a database restored from backup, a bad
//! purge or a lifecycle rule is found by a job rather than by a reader. See
//! DESIGN.md, "Audit".
//!
//! The `audit` job reports and repairs what the read path would repair
//! anyway (a representation whose object is gone is rendered again); it
//! never deletes. `agrafe audit --fix` on the command line is the one that
//! deletes orphan objects, and `--deep` the one that reads the originals
//! back. Like the reaper, each run schedules the next one
//! `jobs.audit_interval` later before doing its work.
//!
//! The listing and the table are merged in id order, one pass over each:
//! the cloud backends list their keys sorted, and the hyphenated lowercase
//! UUID at the head of every key sorts as the `uuid` column does, so the
//! objects of a blob come as one group and the rows between two groups
//! are the ones the listing has nothing for. The local backend's directory
//! walk is sorted in memory, it is for development. Nothing is held for
//! the length of the bucket, in memory or in the database.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroU32;

use agrafe_crypto::streaming::Layout;
use futures::stream::{self, BoxStream};
use futures::{StreamExt as _, TryFutureExt as _, TryStreamExt as _};
use jiff::Timestamp;
use jiff_sqlx::ToSqlx as _;
use object_store::{ObjectMeta, ObjectStoreExt as _};
use sha2::{Digest, Sha256};
use sqlx::{Connection as _, PgConnection, PgExecutor};
use uuid::Uuid;

use super::{Job, JobError, JobKind, Priority};
use crate::app::Agrafe;
use crate::blob::read::{Columns, ReadError, Reader, Stored};
use crate::blob::{self, BlobState, Representation, RepresentationState, repo};
use crate::config::StorageConfig;

/// Why an audit could not run to the end. A mismatch it finds is a
/// finding, not an error.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// Another audit holds the lock: the job on another process, or the
    /// command line.
    #[error("another audit is running")]
    Running,
    /// The store listed this blob's objects after those of a greater id,
    /// which no supported backend does; the audit merges the listing with
    /// the table in id order.
    #[error("the store lists blob {0} out of order; the audit needs the listing sorted by key")]
    Unsorted(Uuid),
    /// Listing the bucket, or deleting an orphan, failed.
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    /// A query failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    /// The deep check could not read an original for a reason other than
    /// the original's own state.
    #[error("reading an original back: {0}")]
    Read(ReadError),
}

/// What an audit may do beyond reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Delete the orphan objects older than `upload.stale_after`, so an
    /// upload in flight is never touched. Off, they are reported only.
    pub fix: bool,
    /// Decrypt one original in `n` (every one for `1`), chosen at random,
    /// and compare its SHA-256 with the row's. Off, no object is read: the
    /// listing's sizes catch truncation on their own.
    pub deep: Option<NonZeroU32>,
}

/// What an audit found. The listing catches a missing or truncated object
/// without a read; the deep check adds what only decrypting shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Finding {
    /// A non-terminal blob's original is gone. Nothing can bring it back;
    /// the row is stamped `failed_at` and `last_error` so the backend sees
    /// it.
    MissingOriginal,
    /// The original is there but is not the file: a length its size does
    /// not produce, or, under `--deep`, bytes that do not decrypt to the
    /// recorded SHA-256. Stamped like a missing one.
    DamagedOriginal,
    /// A representation the row says is `ready` has no object, or one of
    /// the wrong length: set back to `pending` and rendered again.
    MissingRepresentation,
    /// A source part the build of a generated blob still needs is gone or
    /// truncated. Reported; the build fails on its own when it runs.
    MissingSource,
    /// An object under a blob's prefix that no row accounts for: a prefix
    /// without a row, a purged blob's leftovers, an original under a row
    /// that never recorded it, a representation the row says is not
    /// there. Reported, and deleted under `--fix` once older than
    /// `upload.stale_after`.
    Orphan,
    /// A key outside the layout, `<blob id>/<name>`. Reported, never
    /// deleted: it is not agrafe's.
    Foreign,
}

agrafe_types::str_enum!(Finding {
    MissingOriginal => "missing_original",
    DamagedOriginal => "damaged_original",
    MissingRepresentation => "missing_representation",
    MissingSource => "missing_source",
    Orphan => "orphan",
    Foreign => "foreign",
});

/// What one audit saw and did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Objects listed.
    pub objects: u64,
    /// Rows checked: every blob with objects under its prefix, and every
    /// one that should have some.
    pub blobs: u64,
    /// Originals decrypted and hashed, under `--deep`.
    pub deep: u64,
    /// Orphans deleted, under `--fix`.
    pub deleted: u64,
    /// Rows that changed within `upload.stale_after` of the listing
    /// passing their prefix, left for the next run: what the listing
    /// showed of them may predate the change.
    pub unsettled: u64,
    /// Objects no row accounts for, written within `upload.stale_after`
    /// of the listing passing them, left for the next run: an upload or
    /// a render whose row does not say so yet.
    pub young: u64,
    /// Findings by kind, in the order of `Finding::ALL`.
    counts: [u64; Finding::ALL.len()],
}

impl Report {
    /// How many findings of `kind`.
    pub fn count(&self, kind: Finding) -> u64 {
        self.counts[kind as usize]
    }

    /// Findings of every kind.
    pub fn findings(&self) -> u64 {
        self.counts.iter().sum()
    }

    fn add(&mut self, kind: Finding) {
        self.counts[kind as usize] += 1;
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "objects listed:          {}", self.objects)?;
        writeln!(f, "blobs checked:           {}", self.blobs)?;
        writeln!(f, "originals read back:     {}", self.deep)?;
        writeln!(
            f,
            "missing originals:       {}",
            self.count(Finding::MissingOriginal)
        )?;
        writeln!(
            f,
            "damaged originals:       {}",
            self.count(Finding::DamagedOriginal)
        )?;
        writeln!(
            f,
            "missing representations: {}",
            self.count(Finding::MissingRepresentation)
        )?;
        writeln!(
            f,
            "missing source parts:    {}",
            self.count(Finding::MissingSource)
        )?;
        writeln!(
            f,
            "orphan objects:          {}",
            self.count(Finding::Orphan)
        )?;
        writeln!(
            f,
            "foreign objects:         {}",
            self.count(Finding::Foreign)
        )?;
        writeln!(f, "orphans deleted:         {}", self.deleted)?;
        writeln!(f, "rows left for next run:  {}", self.unsettled)?;
        write!(f, "young objects left:      {}", self.young)
    }
}

impl Agrafe {
    /// Check the bucket against the `blobs` table, as the `audit` job does
    /// weekly, with `options` saying what to do beyond reporting. One
    /// finding is one `warn` line in the log; the report is the tally.
    /// [`AuditError::Running`] when another audit holds the lock.
    pub async fn audit(&self, options: Options) -> Result<Report, AuditError> {
        audit(self, options).await
    }
}

pub(crate) async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    if job.kind != JobKind::Audit {
        return Err(JobError::Unhandled(job.kind));
    }
    let now = Timestamp::now();
    schedule(state.pool(), now + state.config().jobs.audit_interval).await?;
    match audit(state, Options::default()).await {
        Ok(report) => {
            tracing::info!(
                objects = report.objects,
                blobs = report.blobs,
                missing_originals = report.count(Finding::MissingOriginal),
                damaged_originals = report.count(Finding::DamagedOriginal),
                missing_representations = report.count(Finding::MissingRepresentation),
                missing_sources = report.count(Finding::MissingSource),
                orphans = report.count(Finding::Orphan),
                foreign = report.count(Finding::Foreign),
                unsettled = report.unsettled,
                young = report.young,
                "audited"
            );
            Ok(())
        }
        Err(AuditError::Running) => {
            tracing::info!("another audit is running, skipped this one");
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

/// Enqueue an audit at `run_at`, unless one is already pending.
pub async fn schedule(exec: impl PgExecutor<'_>, run_at: Timestamp) -> sqlx::Result<bool> {
    super::enqueue(
        exec,
        JobKind::Audit,
        None,
        Priority::Normal,
        serde_json::json!({}),
        run_at,
    )
    .await
}

/// Advisory lock held for the length of one audit, by the session that
/// runs it: the job and the command line never sweep at once. Taken with
/// the database's name as the other half of the key, since advisory locks
/// are the server's and not the database's, and two deployments on one
/// server audit their own buckets.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub const LOCK: i32 = 0x6175_6469; // "audi"

/// Rows read per query: one round trip to the database per thousand
/// blobs, as the listing is one request per thousand objects.
const PAGE: i64 = 1000;

/// Stamp left on a blob whose original the audit found missing or damaged.
const STAMP: &str = "audit: ";

async fn audit(state: &Agrafe, options: Options) -> Result<Report, AuditError> {
    let now = Timestamp::now();
    // One session for the run: it holds the lock and reads the rows, and
    // closing it, however the run ends, releases the lock.
    let mut conn = state.pool().acquire().await?.detach();
    let held: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext(current_database()), $1)")
            .bind(LOCK)
            .fetch_one(&mut conn)
            .await?;
    if !held {
        return Err(AuditError::Running);
    }
    let result = sweep(state, &mut conn, options, now).await;
    // Released here rather than by the close: the server lets go of a
    // closed session's locks only once it has seen the socket go, and an
    // audit started right after this one would find the lock still held.
    // The sweep's outcome is the news; a session that will not unlock or
    // close cleanly is closed by the server when the socket goes.
    let _ = sqlx::query("SELECT pg_advisory_unlock(hashtext(current_database()), $1)")
        .bind(LOCK)
        .execute(&mut conn)
        .await;
    let _ = conn.close().await;
    if let Ok(report) = &result {
        state.metrics().audit(report, Timestamp::now());
    }
    result
}

/// The objects listed under one blob's prefix, as the listing gave them.
struct Group {
    id: Uuid,
    objects: Vec<ObjectMeta>,
    /// When the listing reached the prefix.
    listed_at: Timestamp,
}

async fn sweep(
    state: &Agrafe,
    conn: &mut PgConnection,
    options: Options,
    now: Timestamp,
) -> Result<Report, AuditError> {
    let mut sweep = Sweep {
        state,
        options,
        listed_at: now,
        last: None,
        report: Report::default(),
    };
    let mut rows = Rows::new(conn, now);

    let mut listing = listing(state);
    let mut current: Option<Group> = None;
    while let Some(object) = listing.try_next().await? {
        sweep.report.objects += 1;
        let Some(id) = prefix_of(&object) else {
            sweep.foreign(&object);
            continue;
        };
        match &mut current {
            Some(group) if group.id == id => group.objects.push(object),
            _ => {
                let group = Group {
                    id,
                    objects: vec![object],
                    listed_at: Timestamp::now(),
                };
                if let Some(done) = current.replace(group) {
                    sweep.merge(&mut rows, done).await?;
                }
            }
        }
    }
    if let Some(done) = current {
        sweep.merge(&mut rows, done).await?;
    }

    // The rows after the last prefix listed: the listing passed them all
    // when it ended, with nothing for them.
    sweep.listed_at = Timestamp::now();
    while let Some(row) = rows.next().await? {
        sweep.check(&row, &[]).await?;
    }
    Ok(sweep.report)
}

/// The bucket's objects in key order. The cloud backends list sorted;
/// the local backend walks its directory in whatever order the filesystem
/// gives, so its listing, development-sized, is sorted in memory.
fn listing(state: &Agrafe) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
    let store = state.store().clone();
    if !matches!(state.config().storage, StorageConfig::Local { .. }) {
        return store.list(None);
    }
    async move {
        let mut objects: Vec<ObjectMeta> = store.list(None).try_collect().await?;
        objects.sort_by(|a, b| a.location.cmp(&b.location));
        Ok(stream::iter(objects.into_iter().map(Ok)))
    }
    .try_flatten_stream()
    .boxed()
}

/// The blob an object belongs to: the first segment of its key, when it is
/// a UUID as agrafe writes them, hyphenated and lowercase, and there is a
/// name after it. Any other spelling the parser would take is foreign:
/// `check` matches names under the canonical prefix, and the merge relies
/// on the key order being the id order.
fn prefix_of(object: &ObjectMeta) -> Option<Uuid> {
    let mut parts = object.location.parts();
    let first = parts.next()?;
    parts.next()?;
    let first = first.as_ref();
    let canonical = first.len() == 36
        && first
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-'));
    canonical.then(|| first.parse().ok()).flatten()
}

/// The columns an audit reads of a row, `Row` in the same order. A macro
/// rather than a constant so the queries stay literal strings.
macro_rules! columns {
    () => {
        "id, state, expires_at, storage_key, size, chunk_size, kek_id, wrapped_key, \
         encryption_header, checksum_sha256, source_parts, thumbnail_state, thumbnail_size, \
         preview_state, preview_size, purged_at, updated_at"
    };
}

/// What an audit needs of a row: the state, what objects it accounts for,
/// their expected lengths, and the envelope for the deep check.
#[derive(Debug, sqlx::FromRow)]
struct Row {
    id: Uuid,
    #[sqlx(try_from = "String")]
    state: BlobState,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    expires_at: Option<Timestamp>,
    storage_key: Option<String>,
    size: Option<i64>,
    chunk_size: Option<i32>,
    kek_id: Option<String>,
    wrapped_key: Option<Vec<u8>>,
    encryption_header: Option<Vec<u8>>,
    checksum_sha256: Option<Vec<u8>>,
    source_parts: i32,
    #[sqlx(try_from = "String")]
    thumbnail_state: RepresentationState,
    thumbnail_size: Option<i32>,
    #[sqlx(try_from = "String")]
    preview_state: RepresentationState,
    preview_size: Option<i32>,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    purged_at: Option<Timestamp>,
    /// When the row last changed, by the trigger: a row that changed
    /// after the listing passed its prefix may describe objects the
    /// listing could not have shown.
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    updated_at: Timestamp,
}

impl Row {
    fn layout(&self) -> Option<Layout> {
        self.chunk_size
            .and_then(|s| usize::try_from(s).ok())
            .and_then(|s| Layout::new(s).ok())
    }

    fn representation(&self, kind: Representation) -> (RepresentationState, Option<i32>) {
        match kind {
            Representation::Thumbnail => (self.thumbnail_state, self.thumbnail_size),
            Representation::Preview => (self.preview_state, self.preview_size),
        }
    }
}

/// The rows that require objects, in id order, a page at a time: not
/// terminal, not purged, not expired at the start of the sweep, and
/// naming an original, source parts or a `ready` representation. The
/// rows that require nothing, tombstones among them, are looked up by
/// id when the listing has objects under their prefix.
struct Rows<'c> {
    conn: &'c mut PgConnection,
    now: Timestamp,
    page: VecDeque<Row>,
    cursor: Uuid,
    done: bool,
}

impl<'c> Rows<'c> {
    fn new(conn: &'c mut PgConnection, now: Timestamp) -> Self {
        Self {
            conn,
            now,
            page: VecDeque::new(),
            cursor: Uuid::nil(),
            done: false,
        }
    }

    async fn peek(&mut self) -> sqlx::Result<Option<&Row>> {
        if self.page.is_empty() && !self.done {
            let rows: Vec<Row> = sqlx::query_as(concat!(
                "SELECT ",
                columns!(),
                " FROM blobs \
                 WHERE id > $1 AND purged_at IS NULL \
                   AND state <> ALL($2) AND (expires_at IS NULL OR expires_at > $3) \
                   AND (storage_key IS NOT NULL OR source_parts > 0 \
                        OR thumbnail_state = $4 OR preview_state = $4) \
                 ORDER BY id LIMIT $5"
            ))
            .bind(self.cursor)
            .bind(
                [BlobState::Infected, BlobState::Expired, BlobState::Deleted]
                    .map(BlobState::as_str)
                    .to_vec(),
            )
            .bind(self.now.to_sqlx())
            .bind(RepresentationState::Ready.as_str())
            .bind(PAGE)
            .fetch_all(&mut *self.conn)
            .await?;
            match rows.last() {
                Some(last) => self.cursor = last.id,
                None => self.done = true,
            }
            self.page.extend(rows);
        }
        Ok(self.page.front())
    }

    async fn next(&mut self) -> sqlx::Result<Option<Row>> {
        self.peek().await?;
        Ok(self.page.pop_front())
    }
}

/// The row of `id`, whatever its state.
async fn find(exec: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<Row>> {
    sqlx::query_as(concat!("SELECT ", columns!(), " FROM blobs WHERE id = $1"))
        .bind(id)
        .fetch_optional(exec)
        .await
}

struct Sweep<'a> {
    state: &'a Agrafe,
    options: Options,
    /// When the listing passed the prefixes of the rows being checked. A
    /// row that changed within `upload.stale_after` before it, or after
    /// it, may record an object written after the listing looked, or a
    /// new size for one it listed; an object written within
    /// `upload.stale_after` before it may belong to a row that has not
    /// recorded it yet. Neither is evidence, and both are left for the
    /// next run.
    listed_at: Timestamp,
    /// The last prefix listed, which the next must follow.
    last: Option<Uuid>,
    report: Report,
}

impl Sweep<'_> {
    /// One listed group against the table: the rows before its prefix
    /// have nothing listed, its own row has its objects, and no row at
    /// all makes its objects orphans.
    async fn merge(&mut self, rows: &mut Rows<'_>, group: Group) -> Result<(), AuditError> {
        if self.last.is_some_and(|last| group.id <= last) {
            return Err(AuditError::Unsorted(group.id));
        }
        self.last = Some(group.id);
        self.listed_at = group.listed_at;
        while rows.peek().await?.is_some_and(|row| row.id < group.id) {
            let row = rows.next().await?.expect("peeked");
            self.check(&row, &[]).await?;
        }
        if rows.peek().await?.is_some_and(|row| row.id == group.id) {
            let row = rows.next().await?.expect("peeked");
            return self.check(&row, &group.objects).await;
        }
        // Not among the rows that require objects: a tombstone, a blob
        // awaiting its purge, a `created` one with nothing yet, or no row
        // at all. The lookup also covers a row that appeared after its
        // page was read.
        match find(self.state.pool(), group.id).await? {
            Some(row) => self.check(&row, &group.objects).await,
            None => {
                for object in &group.objects {
                    self.orphan(object, "no row has this id").await?;
                }
                Ok(())
            }
        }
    }

    /// One blob against the objects under its prefix.
    async fn check(&mut self, row: &Row, objects: &[ObjectMeta]) -> Result<(), AuditError> {
        self.report.blobs += 1;
        let state = blob::state_at(row.state, row.expires_at, self.listed_at);
        if row.purged_at.is_some() {
            // A tombstone: the purge took everything, so anything left is
            // an orphan.
            for object in objects {
                self.orphan(object, "the blob was purged").await?;
            }
            return Ok(());
        }
        if state.is_terminal() {
            // Awaiting its purge, which takes the whole prefix: nothing to
            // require and nothing to delete.
            return Ok(());
        }
        // What the row requires is checked only once the row has stood
        // still for `upload.stale_after` before the listing passed it; an
        // object it names may otherwise have been written, or replaced,
        // after the listing looked. An object that young is not judged
        // either, see `young`: the row was read before the listing
        // reached it and may not have recorded the object yet.
        let settled = row.updated_at <= self.settled_before();
        if !settled {
            self.report.unsettled += 1;
        }

        let prefix = format!("{}/", row.id);
        let mut seen: HashMap<&str, &ObjectMeta> = objects
            .iter()
            .filter_map(|o| Some((o.location.as_ref().strip_prefix(prefix.as_str())?, o)))
            .collect();
        let layout = row.layout();

        if let Some(key) = &row.storage_key {
            let name = key.strip_prefix(prefix.as_str()).unwrap_or(key);
            match (seen.remove(name), settled) {
                (None, true) => {
                    self.stamp(
                        row,
                        Finding::MissingOriginal,
                        "the original is missing from the store".to_owned(),
                    )
                    .await?;
                }
                (Some(object), true) => {
                    let want = layout
                        .zip(row.size)
                        .map(|(layout, size)| layout.ciphertext_len(size as u64));
                    match want {
                        Some(want) if want != object.size && !self.young(object) => {
                            self.stamp(
                                row,
                                Finding::DamagedOriginal,
                                format!(
                                    "the original is {} bytes, not the {want} its size calls for",
                                    object.size
                                ),
                            )
                            .await?;
                        }
                        _ => {
                            if self.sampled() {
                                self.deep(row).await?;
                            }
                        }
                    }
                }
                (_, false) => {}
            }
        }

        for kind in [Representation::Thumbnail, Representation::Preview] {
            let (rep_state, size) = row.representation(kind);
            match (seen.remove(kind.as_str()), rep_state) {
                (Some(object), RepresentationState::Ready) => {
                    let want = layout
                        .zip(size)
                        .map(|(layout, size)| layout.ciphertext_len(size as u64));
                    if let Some(want) = want
                        && want != object.size
                        && settled
                        && !self.young(object)
                    {
                        self.rerender(
                            row,
                            kind,
                            &format!("{} bytes, not the {want} its size calls for", object.size),
                        )
                        .await?;
                    }
                }
                (None, RepresentationState::Ready) => {
                    if state == BlobState::Ready && settled {
                        self.rerender(row, kind, "missing from the store").await?;
                    }
                }
                // A render is going to replace it.
                (Some(_), RepresentationState::Pending) => {}
                (Some(object), _) => {
                    self.orphan(object, &format!("the row says the {kind} is {rep_state}"))
                        .await?;
                }
                (None, _) => {}
            }
        }

        // The parts stay after the build and go with the purge; before it,
        // the build needs every one.
        let needed = settled && matches!(state, BlobState::Created | BlobState::Uploaded);
        for n in 1..=u32::try_from(row.source_parts).unwrap_or(0) {
            let name = format!("source/{n}");
            match seen.remove(name.as_str()) {
                Some(object) => {
                    if let Some(layout) = layout
                        && layout.plaintext_len(object.size).is_err()
                        && settled
                        && !self.young(object)
                    {
                        self.missing_source(
                            row,
                            n,
                            &format!(
                                "is {} bytes, which no stream of its layout produces",
                                object.size
                            ),
                        );
                    }
                }
                None if needed => self.missing_source(row, n, "is missing from the store"),
                None => {}
            }
        }

        let rest: Vec<&ObjectMeta> = seen.into_values().collect();
        for object in rest {
            self.orphan(object, "the row does not account for it")
                .await?;
        }
        Ok(())
    }

    /// A row that last changed before this stood still for
    /// `upload.stale_after` before the listing passed its prefix, and an
    /// object written before this was there for as long.
    fn settled_before(&self) -> Timestamp {
        self.listed_at
            .checked_sub(self.state.config().upload.stale_after)
            .unwrap_or(Timestamp::MIN)
    }

    /// Whether the object was written within `upload.stale_after` of the
    /// listing passing it: an upload or a render in flight, whose row may
    /// not say so yet.
    fn young(&self, object: &ObjectMeta) -> bool {
        object.last_modified.timestamp() > self.settled_before().as_second()
    }

    /// Whether this original is in the deep sample.
    fn sampled(&self) -> bool {
        match self.options.deep {
            None => false,
            Some(n) if n.get() == 1 => true,
            Some(n) => Uuid::new_v4().as_u128().is_multiple_of(u128::from(n.get())),
        }
    }

    /// Decrypt the original and compare its SHA-256 with the row's. What
    /// the row or the object is at fault for is a finding; a store that
    /// cannot be read is an error.
    async fn deep(&mut self, row: &Row) -> Result<(), AuditError> {
        self.report.deep += 1;
        match read_back(self.state, row).await {
            Ok(digest) if row.checksum_sha256.as_deref() == Some(&digest[..]) => Ok(()),
            Ok(digest) => {
                self.stamp(
                    row,
                    Finding::DamagedOriginal,
                    format!(
                        "the original decrypts to a file with SHA-256 {}, not the one recorded",
                        hex(&digest)
                    ),
                )
                .await
            }
            Err(ReadError::Store(err)) if !matches!(err, object_store::Error::NotFound { .. }) => {
                Err(AuditError::Read(ReadError::Store(err)))
            }
            Err(err) => {
                self.stamp(
                    row,
                    Finding::DamagedOriginal,
                    format!("the original does not read back: {err}"),
                )
                .await
            }
        }
    }

    /// Record a finding on the original: the log line, the count, and the
    /// row's `failed_at` and `last_error` unless a verdict is on it
    /// already, or the row moved on since it was read. Nothing clears
    /// the stamp but a retry, which a blob with its original gone has
    /// nothing to retry; the first one stands.
    async fn stamp(&mut self, row: &Row, finding: Finding, what: String) -> Result<(), AuditError> {
        self.report.add(finding);
        tracing::warn!(blob = %row.id, finding = %finding, "audit: {what}");
        sqlx::query(
            "UPDATE blobs SET failed_at = now(), last_error = $2 \
             WHERE id = $1 AND failed_at IS NULL AND state = $3 AND purged_at IS NULL",
        )
        .bind(row.id)
        .bind(format!("{STAMP}{what}"))
        .bind(row.state.as_str())
        .execute(self.state.pool())
        .await?;
        Ok(())
    }

    /// The representation is not what the row says: back to `pending`
    /// and rendered again, as the read path does when it finds the object
    /// gone.
    async fn rerender(
        &mut self,
        row: &Row,
        kind: Representation,
        what: &str,
    ) -> Result<(), AuditError> {
        self.report.add(Finding::MissingRepresentation);
        tracing::warn!(blob = %row.id, finding = %Finding::MissingRepresentation, "audit: the {kind} is {what}, rendering it again");
        repo::request_representation(self.state.pool(), row.id, kind).await?;
        self.state.heads().invalidate(row.id);
        Ok(())
    }

    fn missing_source(&mut self, row: &Row, n: u32, what: &str) {
        self.report.add(Finding::MissingSource);
        tracing::warn!(blob = %row.id, finding = %Finding::MissingSource, "audit: source part {n} {what}");
    }

    fn foreign(&mut self, object: &ObjectMeta) {
        self.report.add(Finding::Foreign);
        tracing::warn!(key = %object.location, size = object.size, finding = %Finding::Foreign, "audit: object outside the layout");
    }

    /// An object no row accounts for: reported, and under `--fix` deleted.
    /// One younger than `upload.stale_after` is left for the next run: an
    /// upload or a render that has written its object and not yet updated
    /// its row looks the same.
    async fn orphan(&mut self, object: &ObjectMeta, why: &str) -> Result<(), AuditError> {
        if self.young(object) {
            self.report.young += 1;
            return Ok(());
        }
        self.report.add(Finding::Orphan);
        if self.options.fix {
            match self.state.store().delete(&object.location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(err) => return Err(err.into()),
            }
            self.report.deleted += 1;
            tracing::warn!(key = %object.location, size = object.size, finding = %Finding::Orphan, "audit: orphan object deleted, {why}");
        } else {
            tracing::warn!(key = %object.location, size = object.size, modified = %object.last_modified, finding = %Finding::Orphan, "audit: orphan object, {why}");
        }
        Ok(())
    }
}

/// Decrypt the whole original and hash the plaintext.
async fn read_back(state: &Agrafe, row: &Row) -> Result<[u8; 32], ReadError> {
    let stored = Stored::from_columns(Columns {
        blob_id: row.id,
        storage_key: row.storage_key.as_deref(),
        kek_id: row.kek_id.as_deref(),
        wrapped_key: row.wrapped_key.as_deref(),
        encryption_header: row.encryption_header.as_deref(),
        chunk_size: row.chunk_size,
        checksum_sha256: row.checksum_sha256.as_deref(),
        size: row.size,
    })?;
    let mut reader = Reader::open(state, &stored, 0..stored.size).await?;
    let mut hasher = Sha256::new();
    while let Some(bytes) = reader.next_segment().await? {
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_tallies_by_kind() {
        let mut report = Report::default();
        report.add(Finding::Orphan);
        report.add(Finding::Orphan);
        report.add(Finding::Foreign);
        assert_eq!(report.count(Finding::Orphan), 2);
        assert_eq!(report.count(Finding::Foreign), 1);
        assert_eq!(report.count(Finding::MissingOriginal), 0);
        assert_eq!(report.findings(), 3);
        let text = report.to_string();
        assert!(text.contains("orphan objects:          2"), "{text}");
    }

    #[test]
    fn a_prefix_is_a_uuid_with_a_name_after_it() {
        let meta = |key: &str| ObjectMeta {
            location: object_store::path::Path::from(key),
            last_modified: Default::default(),
            size: 0,
            e_tag: None,
            version: None,
        };
        let id = Uuid::now_v7();
        assert_eq!(prefix_of(&meta(&format!("{id}/original"))), Some(id));
        assert_eq!(prefix_of(&meta(&format!("{id}/source/3"))), Some(id));
        assert_eq!(prefix_of(&meta(&id.to_string())), None, "no name");
        assert_eq!(prefix_of(&meta("junk/original")), None);
        assert_eq!(
            prefix_of(&meta(&format!("{}/original", id.as_simple()))),
            None,
            "not the spelling agrafe writes"
        );
        assert_eq!(
            prefix_of(&meta(&format!(
                "{}/original",
                id.to_string().to_uppercase()
            ))),
            None
        );
        assert_eq!(prefix_of(&meta("README")), None);
    }
}
