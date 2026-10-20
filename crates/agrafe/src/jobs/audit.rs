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
//! The listing is streamed in whatever order the store gives it and the
//! objects of a blob are taken as one group, which holds for the sorted
//! listings of the cloud backends and for the local backend's directory
//! walk alike. The prefixes seen go into a temporary table of the session
//! the audit holds, and the rows holding objects that never came up are
//! read back from it at the end: no order is assumed, and nothing is held
//! in memory for the length of the bucket.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU32;

use agrafe_crypto::streaming::Layout;
use futures::TryStreamExt as _;
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

/// Why an audit could not run to the end. A mismatch it finds is a
/// finding, not an error.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// Another audit holds the lock: the job on another process, or the
    /// command line.
    #[error("another audit is running")]
    Running,
    /// The store listed the objects of one blob in more than one run,
    /// which no supported backend does; the audit needs them grouped.
    #[error("the store lists the objects of blob {0} apart; the audit needs them together")]
    Scattered(Uuid),
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
        write!(f, "orphans deleted:         {}", self.deleted)
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

/// Prefixes looked up per query: one round trip to the database per
/// thousand blobs listed, as the listing is one request per thousand
/// objects.
const BATCH: usize = 1000;

/// Stamp left on a blob whose original the audit found missing or damaged.
const STAMP: &str = "audit: ";

async fn audit(state: &Agrafe, options: Options) -> Result<Report, AuditError> {
    let now = Timestamp::now();
    // One session for the run: it holds the lock and the table of prefixes
    // seen, and closing it, however the run ends, releases both.
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
}

async fn sweep(
    state: &Agrafe,
    conn: &mut PgConnection,
    options: Options,
    now: Timestamp,
) -> Result<Report, AuditError> {
    sqlx::query("CREATE TEMP TABLE audit_seen (id uuid PRIMARY KEY)")
        .execute(&mut *conn)
        .await?;
    let stale_before = now
        .checked_sub(state.config().upload.stale_after)
        .unwrap_or(Timestamp::MIN);
    let mut sweep = Sweep {
        state,
        options,
        now,
        stale_before,
        report: Report::default(),
    };

    let mut listing = state.store().list(None);
    let mut batch: Vec<Group> = Vec::new();
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
                };
                if let Some(done) = current.replace(group) {
                    batch.push(done);
                }
                if batch.len() >= BATCH {
                    sweep.check_batch(conn, std::mem::take(&mut batch)).await?;
                }
            }
        }
    }
    batch.extend(current);
    sweep.check_batch(conn, batch).await?;

    // The rows that should have objects and whose prefix never came up.
    let mut cursor = Uuid::nil();
    loop {
        let rows = unseen(conn, cursor, now).await?;
        let Some(last) = rows.last() else { break };
        cursor = last.id;
        for row in &rows {
            sweep.check(row, &[]).await?;
        }
    }
    Ok(sweep.report)
}

/// The blob an object belongs to: the first segment of its key, when it is
/// a UUID and there is a name after it.
fn prefix_of(object: &ObjectMeta) -> Option<Uuid> {
    let mut parts = object.location.parts();
    let first = parts.next()?;
    parts.next()?;
    first.as_ref().parse().ok()
}

/// The columns an audit reads of a row, `Row` in the same order. A macro
/// rather than a constant so the queries stay literal strings.
macro_rules! columns {
    () => {
        "id, state, expires_at, storage_key, size, chunk_size, kek_id, wrapped_key, \
         encryption_header, checksum_sha256, source_parts, thumbnail_state, thumbnail_size, \
         preview_state, preview_size, purged_at"
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

/// The rows of `ids`, by id.
async fn rows(conn: &mut PgConnection, ids: &[Uuid]) -> sqlx::Result<HashMap<Uuid, Row>> {
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM blobs WHERE id = ANY($1)"
    ))
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|row| (row.id, row)).collect())
}

/// A page of the rows after `cursor` that should have objects in the
/// bucket, not terminal, not purged, and whose prefix the listing never
/// showed: the ones the merge over the listing could not have checked.
async fn unseen(conn: &mut PgConnection, cursor: Uuid, now: Timestamp) -> sqlx::Result<Vec<Row>> {
    sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM blobs b \
         WHERE b.id > $1 AND b.purged_at IS NULL \
           AND b.state <> ALL($2) AND (b.expires_at IS NULL OR b.expires_at > $3) \
           AND (b.storage_key IS NOT NULL OR b.source_parts > 0 \
                OR b.thumbnail_state = $4 OR b.preview_state = $4) \
           AND NOT EXISTS (SELECT 1 FROM audit_seen s WHERE s.id = b.id) \
         ORDER BY b.id LIMIT $5"
    ))
    .bind(cursor)
    .bind(
        [BlobState::Infected, BlobState::Expired, BlobState::Deleted]
            .map(BlobState::as_str)
            .to_vec(),
    )
    .bind(now.to_sqlx())
    .bind(RepresentationState::Ready.as_str())
    .bind(BATCH as i64)
    .fetch_all(&mut *conn)
    .await
}

struct Sweep<'a> {
    state: &'a Agrafe,
    options: Options,
    now: Timestamp,
    /// An orphan modified before this may be deleted; a younger one may be
    /// an upload or a render that has not updated its row yet.
    stale_before: Timestamp,
    report: Report,
}

impl Sweep<'_> {
    /// Look the rows of `groups` up in one query, record the prefixes as
    /// seen, and check each group against its row.
    async fn check_batch(
        &mut self,
        conn: &mut PgConnection,
        groups: Vec<Group>,
    ) -> Result<(), AuditError> {
        if groups.is_empty() {
            return Ok(());
        }
        let mut ids = HashSet::with_capacity(groups.len());
        for group in &groups {
            if !ids.insert(group.id) {
                return Err(AuditError::Scattered(group.id));
            }
        }
        let ids: Vec<Uuid> = ids.into_iter().collect();
        let inserted: Vec<Uuid> = sqlx::query_scalar(
            "INSERT INTO audit_seen (id) SELECT unnest($1::uuid[]) \
             ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?;
        if inserted.len() != ids.len() {
            let inserted: HashSet<Uuid> = inserted.into_iter().collect();
            let again = ids.iter().find(|id| !inserted.contains(id)).copied();
            return Err(AuditError::Scattered(again.unwrap_or_default()));
        }
        let rows = rows(conn, &ids).await?;
        for group in &groups {
            match rows.get(&group.id) {
                Some(row) => self.check(row, &group.objects).await?,
                None => {
                    for object in &group.objects {
                        self.orphan(object, "no row has this id").await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// One blob against the objects under its prefix.
    async fn check(&mut self, row: &Row, objects: &[ObjectMeta]) -> Result<(), AuditError> {
        self.report.blobs += 1;
        let state = blob::state_at(row.state, row.expires_at, self.now);
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

        let prefix = format!("{}/", row.id);
        let mut seen: HashMap<&str, &ObjectMeta> = objects
            .iter()
            .filter_map(|o| Some((o.location.as_ref().strip_prefix(prefix.as_str())?, o)))
            .collect();
        let layout = row.layout();

        if let Some(key) = &row.storage_key {
            let name = key.strip_prefix(prefix.as_str()).unwrap_or(key);
            match seen.remove(name) {
                None => {
                    self.stamp(
                        row,
                        Finding::MissingOriginal,
                        "the original is missing from the store".to_owned(),
                    )
                    .await?;
                }
                Some(object) => {
                    let want = layout
                        .zip(row.size)
                        .map(|(layout, size)| layout.ciphertext_len(size as u64));
                    match want {
                        Some(want) if want != object.size => {
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
                    if state == BlobState::Ready {
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
        let needed = matches!(state, BlobState::Created | BlobState::Uploaded);
        for n in 1..=u32::try_from(row.source_parts).unwrap_or(0) {
            let name = format!("source/{n}");
            match seen.remove(name.as_str()) {
                Some(object) => {
                    if let Some(layout) = layout
                        && layout.plaintext_len(object.size).is_err()
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
    /// already. Nothing clears the stamp but a retry, which a blob with
    /// its original gone has nothing to retry; the first one stands.
    async fn stamp(&mut self, row: &Row, finding: Finding, what: String) -> Result<(), AuditError> {
        self.report.add(finding);
        tracing::warn!(blob = %row.id, finding = %finding, "audit: {what}");
        sqlx::query(
            "UPDATE blobs SET failed_at = now(), last_error = $2 \
             WHERE id = $1 AND failed_at IS NULL",
        )
        .bind(row.id)
        .bind(format!("{STAMP}{what}"))
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

    /// An object no row accounts for: reported, and under `--fix` deleted
    /// once it is older than `upload.stale_after`.
    async fn orphan(&mut self, object: &ObjectMeta, why: &str) -> Result<(), AuditError> {
        self.report.add(Finding::Orphan);
        let old = object.last_modified.timestamp() <= self.stale_before.as_second();
        if self.options.fix && old {
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
        assert_eq!(prefix_of(&meta("README")), None);
    }
}
