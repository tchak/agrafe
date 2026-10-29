//! The reconciliation sweep: the bucket listing against the `blobs` table,
//! both read whole, so a bucket or a database restored from backup, a bad
//! purge or a lifecycle rule is found by a job rather than by a reader. See
//! DESIGN.md, "Audit".
//!
//! The `audit` job reports and repairs what the read path would repair
//! anyway (a representation whose object is gone is rendered again); it
//! never deletes. `agrafe audit --fix` on the command line is the one that
//! deletes orphan objects, `--delete-unknown` the one that deletes the
//! objects under a prefix no row has, which after a database restored
//! from backup are the blobs the backup missed, and `--deep` the one that
//! reads the originals back. Each run schedules the next one
//! `jobs.audit_interval` after it finishes.
//!
//! A sweep of a large bucket takes days, and the job keeps its place: the
//! last key listed, the last row passed and the tally so far go into the
//! job's payload every minute (`Progress`), and the attempt that finds
//! them there carries on from that point, after a restart that let the
//! lease lapse or a failure the runner retries. Acting on a finding never
//! stops the sweep: a stamp, a render or a delete that fails is logged,
//! counted in the report and left for the next run; only the listing and
//! the rows failing ends an attempt.
//!
//! The listing and the table are merged in key order, one pass over each:
//! the cloud backends list their keys sorted, and a key is the blob's shard
//! then its hyphenated lowercase id (`blob::key`), which the rows are paged
//! in through an expression index on the same two, so the objects of a
//! blob come as one group and the rows between two groups are the ones the
//! listing has nothing for. The local backend's directory walk is sorted in
//! memory, it is for development. Nothing is held for the length of the
//! bucket, in memory or in the database.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use agrafe_crypto::streaming::Layout;
use futures::stream::{self, BoxStream};
use futures::{StreamExt as _, TryFutureExt as _, TryStreamExt as _};
use jiff::Timestamp;
use jiff_sqlx::ToSqlx as _;
use object_store::ObjectMeta;
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection as _, PgConnection, PgExecutor};
use uuid::Uuid;

use super::{Job, JobError, JobKind, Priority};
use crate::app::Agrafe;
use crate::blob::read::{Columns, ReadError, Reader, Stored};
use crate::blob::{self, BlobState, Representation, RepresentationState, key, repo};
use crate::config::StorageConfig;

/// Why an audit could not run to the end. A mismatch it finds is a
/// finding, not an error.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// Another audit holds the lock: the job on another process, or the
    /// command line.
    #[error("another audit is running")]
    Running,
    /// The store listed this blob's objects after those of a greater key,
    /// which no supported backend does; the audit merges the listing with
    /// the table in key order (`blob::key::order`).
    #[error("the store lists blob {0} out of order; the audit needs the listing sorted by key")]
    Unsorted(Uuid),
    /// Listing the bucket failed.
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    /// A query failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// What an audit may do beyond reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Delete the orphan objects older than `upload.stale_after`, so an
    /// upload in flight is never touched: the objects a row accounts
    /// against, never those of a prefix no row has. Off, they are
    /// reported only.
    pub fix: bool,
    /// Delete the objects under a prefix no row has, older than
    /// `upload.stale_after`. Its own switch, since after a database
    /// restored from backup these are the blobs the backup missed, rows
    /// to recover rather than objects to delete.
    pub delete_unknown: bool,
    /// Decrypt one original in `n` (every one for `1`), picked by id so
    /// the sample is the same from one run to the next, and compare its
    /// SHA-256 with the row's. Off, no object is read: the listing's
    /// sizes catch truncation on their own.
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
    /// An object under a known blob's prefix that its row does not
    /// account for: a purged blob's leftovers, an original under a row
    /// that never recorded it, a representation the row says is not
    /// there. The row is authoritative, so it is garbage: reported, and
    /// deleted under `--fix` once older than `upload.stale_after`.
    Orphan,
    /// An object under a prefix no row has. Not the same thing: the row
    /// may have been lost, which is what a database restored from a
    /// backup older than the blob looks like, and the object is then the
    /// file, still served through the envelope the backend holds. Also a
    /// purge's leftover whose tombstone has since been pruned. Reported
    /// with the span of such prefixes, and deleted only under
    /// `--delete-unknown`.
    Unknown,
    /// A key outside the layout, `<shard>/<blob id>/<name>`. Reported, never
    /// deleted: it is not agrafe's.
    Foreign,
}

agrafe_types::str_enum!(Finding {
    MissingOriginal => "missing_original",
    DamagedOriginal => "damaged_original",
    MissingRepresentation => "missing_representation",
    MissingSource => "missing_source",
    Orphan => "orphan",
    Unknown => "unknown",
    Foreign => "foreign",
});

/// The prefixes no row has, as a span. The ids are UUID v7, so the first
/// and the last say when the blobs were created, and after a database
/// restored from backup, what the backup missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unknown {
    /// Prefixes with objects older than `upload.stale_after` and no row.
    pub prefixes: u64,
    /// The lowest id among them.
    pub first: Uuid,
    /// The highest.
    pub last: Uuid,
}

impl Unknown {
    /// When `first` and `last` were created, for ids that carry a time.
    fn created(&self) -> Option<(Timestamp, Timestamp)> {
        created_at(self.first).zip(created_at(self.last))
    }
}

impl fmt::Display for Unknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} prefixes, ", self.prefixes)?;
        match self.created() {
            Some((first, last)) => write!(
                f,
                "blobs created between {} and {}",
                first.strftime("%FT%TZ"),
                last.strftime("%FT%TZ")
            ),
            None => write!(f, "ids {} to {}", self.first, self.last),
        }
    }
}

/// When a UUID v7 was made.
fn created_at(id: Uuid) -> Option<Timestamp> {
    let (seconds, nanos) = id.get_timestamp()?.to_unix();
    Timestamp::new(i64::try_from(seconds).ok()?, i32::try_from(nanos).ok()?).ok()
}

/// What one audit saw and did. Serialized into the job's payload as part
/// of its `Progress`, so every field has a default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// Objects listed.
    pub objects: u64,
    /// Rows checked: every blob with objects under its prefix, and every
    /// one that should have some.
    pub blobs: u64,
    /// Originals decrypted and hashed, under `--deep`.
    pub deep: u64,
    /// Objects deleted, under `--fix` and `--delete-unknown`.
    pub deleted: u64,
    /// The prefixes no row has, when there are any: how many, and the
    /// span of their ids. The finding count is the objects under them.
    pub unknown: Option<Unknown>,
    /// Rows that changed within `upload.stale_after` of the listing
    /// passing their prefix, left for the next run: what the listing
    /// showed of them may predate the change.
    pub unsettled: u64,
    /// Objects no row accounts for, written within `upload.stale_after`
    /// of the listing passing them, left for the next run: an upload or
    /// a render whose row does not say so yet.
    pub young: u64,
    /// Findings the audit could not act on: a stamp, a render request or
    /// a delete that failed, or an original the deep check could not
    /// read. Each is an `error` line in the log, and the next run finds
    /// it again.
    pub errors: u64,
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

    /// Findings of every kind but `foreign`: the ones where the table and
    /// the bucket disagree about agrafe's own objects, a prefix no row
    /// has among them. What the command line exits non-zero on; a key
    /// that is not agrafe's is reported and is no reason to fail a
    /// restore procedure.
    pub fn mismatches(&self) -> u64 {
        self.findings() - self.count(Finding::Foreign)
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
            "objects with no row:     {}",
            self.count(Finding::Unknown)
        )?;
        if let Some(unknown) = &self.unknown {
            writeln!(f, "  under {unknown}")?;
        }
        writeln!(
            f,
            "foreign objects:         {}",
            self.count(Finding::Foreign)
        )?;
        writeln!(f, "objects deleted:         {}", self.deleted)?;
        writeln!(f, "rows left for next run:  {}", self.unsettled)?;
        writeln!(f, "young objects left:      {}", self.young)?;
        write!(f, "findings not acted on:   {}", self.errors)
    }
}

impl Agrafe {
    /// Check the bucket against the `blobs` table, as the `audit` job does
    /// weekly, with `options` saying what to do beyond reporting. One
    /// finding is one `warn` line in the log; the report is the tally.
    /// [`AuditError::Running`] when another audit holds the lock.
    pub async fn audit(&self, options: Options) -> Result<Report, AuditError> {
        audit(self, options, Progress::default(), None).await
    }
}

/// Where a sweep stands, kept in the job's payload so the next attempt
/// carries on from there; `{}` is a sweep not started.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    /// The last key listed and checked; the listing resumes after it.
    pub after: Option<String>,
    /// The last row the listing passed; the rows resume after it, in key
    /// order (its shard is computed from it).
    pub cursor: Uuid,
    /// The tally so far.
    pub report: Report,
}

/// How often a running job saves its `Progress`.
const CHECKPOINT_EVERY: Duration = Duration::from_secs(60);

/// Objects deleted per request, under `--fix` and `--delete-unknown`: S3
/// takes a thousand in one `DeleteObjects`, the other backends delete
/// them one by one anyway.
const DELETE_BATCH: usize = 1000;

/// The job the sweep saves its progress to, as the worker that holds it.
struct Checkpoint {
    job: i64,
    worker: String,
    saved: Instant,
}

pub(crate) async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    if job.kind != JobKind::Audit {
        return Err(JobError::Unhandled(job.kind));
    }
    let progress = match serde_json::from_value::<Progress>(job.payload.clone()) {
        Ok(progress) => progress,
        Err(err) => {
            tracing::warn!(error = %err, "the audit job's progress does not parse, starting over");
            Progress::default()
        }
    };
    let resuming = progress.after.is_some() || !progress.cursor.is_nil();
    if resuming {
        tracing::info!(after = ?progress.after, cursor = %progress.cursor, "resuming the audit");
    }
    let checkpoint = job.locked_by.clone().map(|worker| Checkpoint {
        job: job.id,
        worker,
        saved: Instant::now(),
    });
    let result = audit(state, Options::default(), progress, checkpoint).await;
    // The next run is scheduled once this one is over, not before: a
    // pending twin would make a failed attempt redundant instead of
    // retried (`fail`), and the retry is what carries the progress on.
    // The last attempt and a run that stepped aside schedule it too, so
    // a dead job does not end the weekly sweep.
    let next = Timestamp::now() + state.config().jobs.audit_interval;
    match result {
        Ok(report) => {
            schedule_after(state.pool(), next, Some(job.id)).await?;
            tracing::info!(
                objects = report.objects,
                blobs = report.blobs,
                missing_originals = report.count(Finding::MissingOriginal),
                damaged_originals = report.count(Finding::DamagedOriginal),
                missing_representations = report.count(Finding::MissingRepresentation),
                missing_sources = report.count(Finding::MissingSource),
                orphans = report.count(Finding::Orphan),
                unknown = report.count(Finding::Unknown),
                unknown_prefixes = report.unknown.as_ref().map(ToString::to_string),
                foreign = report.count(Finding::Foreign),
                unsettled = report.unsettled,
                young = report.young,
                errors = report.errors,
                "audited"
            );
            Ok(())
        }
        // Another sweep holds the lock: the command line, or this job's own
        // previous attempt, whose session outlives its lapsed lease until
        // the server sees the socket go. A sweep with progress keeps it and
        // is retried, below; a fresh one has nothing to keep.
        Err(AuditError::Running) if !resuming => {
            schedule_after(state.pool(), next, Some(job.id)).await?;
            tracing::info!("another audit is running, skipped this one");
            Ok(())
        }
        Err(err) => {
            if job.attempts >= job.max_attempts {
                schedule_after(state.pool(), next, Some(job.id)).await?;
            }
            Err(err.into())
        }
    }
}

/// Enqueue an audit at `run_at`, unless one is live already: pending,
/// running, or locked by a worker whose lease lapsed, which a restart
/// leaves behind. A sweep in progress is one, its progress in its row: a
/// twin beside it would supersede it at its first failure, and the
/// progress would go with it. Outside any trace context, as
/// `reap::schedule`: each run starts a trace.
pub async fn schedule(exec: impl PgExecutor<'_>, run_at: Timestamp) -> sqlx::Result<bool> {
    schedule_after(exec, run_at, None).await
}

/// `schedule`, for the run `current` scheduling its successor: its own
/// row, still there until it succeeds, does not count.
async fn schedule_after(
    exec: impl PgExecutor<'_>,
    run_at: Timestamp,
    current: Option<i64>,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "WITH inserted AS (\
             INSERT INTO jobs (kind, priority, payload, run_at) \
             SELECT $1, $2, '{}', $3 \
             WHERE NOT EXISTS (\
                 SELECT 1 FROM jobs \
                 WHERE kind = $1 AND blob_id IS NULL AND failed_at IS NULL \
                   AND id IS DISTINCT FROM $4\
             ) \
             ON CONFLICT DO NOTHING RETURNING kind\
         ) \
         SELECT pg_notify($5, kind) FROM inserted",
    )
    .bind(JobKind::Audit.as_str())
    .bind(Priority::Normal.rank())
    .bind(run_at.to_sqlx())
    .bind(current)
    .bind(super::CHANNEL)
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Advisory lock held for the length of one audit, by the session that
/// runs it: the job and the command line never sweep at once. Keyed on
/// the installation (see `jobs::try_lock`), so two deployments on one
/// server audit their own buckets.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub const LOCK: i32 = 0x6175_6469; // "audi"

/// Rows read per query: one round trip to the database per thousand
/// blobs, as the listing is one request per thousand objects.
const PAGE: i64 = 1000;

/// Stamp left on a blob whose original the audit found missing or damaged.
pub(crate) const STAMP: &str = "audit: ";

async fn audit(
    state: &Agrafe,
    options: Options,
    progress: Progress,
    checkpoint: Option<Checkpoint>,
) -> Result<Report, AuditError> {
    let now = Timestamp::now();
    // One session for the run: it holds the lock and reads the rows, and
    // closing it, however the run ends, releases the lock.
    let mut conn = state.pool().acquire().await?.detach();
    if !super::try_lock(&mut conn, LOCK).await? {
        return Err(AuditError::Running);
    }
    let result = sweep(state, &mut conn, options, now, progress, checkpoint).await;
    // Released here rather than by the close: the server lets go of a
    // closed session's locks only once it has seen the socket go, and an
    // audit started right after this one would find the lock still held.
    // The sweep's outcome is the news; a session that will not unlock or
    // close cleanly is closed by the server when the socket goes.
    let _ = super::unlock(&mut conn, LOCK).await;
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
    progress: Progress,
    checkpoint: Option<Checkpoint>,
) -> Result<Report, AuditError> {
    let after = progress.after.map(Path::from);
    let mut sweep = Sweep {
        state,
        options,
        listed_at: now,
        last: after.as_ref().and_then(key::owner),
        after: after.clone(),
        cursor: progress.cursor,
        checkpoint,
        pending: Vec::new(),
        report: progress.report,
    };
    let mut rows = Rows::new(conn, now, progress.cursor);

    let mut listing = listing(state, after);
    let mut current: Option<Group> = None;
    while let Some(object) = listing.try_next().await? {
        sweep.report.objects += 1;
        let Some(id) = key::owner(&object.location) else {
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
    // The listing is done, which is most of the work: worth saving.
    sweep.save(true).await;

    // The rows after the last prefix listed: the listing passed them all
    // when it ended, with nothing for them.
    sweep.listed_at = Timestamp::now();
    while let Some(row) = rows.next().await? {
        sweep.check(&row, &[]).await;
        sweep.cursor = row.id;
        sweep.save(false).await;
    }
    sweep.delete_pending().await;
    Ok(sweep.report)
}

/// The bucket's objects in key order, after `after` when resuming. The
/// cloud backends list sorted, and S3 and GCS start after a key on the
/// server (Azure lists from the start and the rest is skipped here); the
/// local backend walks its directory in whatever order the filesystem
/// gives, so its listing, development-sized, is sorted in memory.
fn listing(
    state: &Agrafe,
    after: Option<Path>,
) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
    let store = state.store().clone();
    if !matches!(state.config().storage, StorageConfig::Local { .. }) {
        return match after {
            Some(after) => store.list_with_offset(None, &after),
            None => store.list(None),
        };
    }
    async move {
        let mut objects: Vec<ObjectMeta> = store.list(None).try_collect().await?;
        objects.sort_by(|a, b| a.location.cmp(&b.location));
        if let Some(after) = after {
            objects.retain(|o| o.location > after);
        }
        Ok(stream::iter(objects.into_iter().map(Ok)))
    }
    .try_flatten_stream()
    .boxed()
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

/// The rows that require objects, in key order (the shard, then the id, see
/// `blob::key::order`; `blobs_shard_idx`), a page at a time: not
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
    fn new(conn: &'c mut PgConnection, now: Timestamp, after: Uuid) -> Self {
        Self {
            conn,
            now,
            page: VecDeque::new(),
            cursor: after,
            done: false,
        }
    }

    async fn peek(&mut self) -> sqlx::Result<Option<&Row>> {
        if self.page.is_empty() && !self.done {
            let rows: Vec<Row> = sqlx::query_as(concat!(
                "SELECT ",
                columns!(),
                " FROM blobs \
                 WHERE (get_byte(uuid_send(id), 15), id) > ($1, $2) AND purged_at IS NULL \
                   AND state <> ALL($3) AND (expires_at IS NULL OR expires_at > $4) \
                   AND (storage_key IS NOT NULL OR source_parts > 0 \
                        OR thumbnail_state = $5 OR preview_state = $5) \
                 ORDER BY get_byte(uuid_send(id), 15), id LIMIT $6"
            ))
            .bind(i32::from(key::shard(self.cursor)))
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
    /// The last prefix listed, which the next must follow in key order;
    /// on a resumed attempt, the owner of the key the listing resumes
    /// after, so the first prefix listed is held to the order too.
    last: Option<Uuid>,
    /// The last key checked and the last row passed, what a resumed
    /// attempt starts after.
    after: Option<Path>,
    cursor: Uuid,
    /// The job to save them to, when running as one.
    checkpoint: Option<Checkpoint>,
    /// Objects to delete, under `--fix` and `--delete-unknown`, in
    /// batches of `DELETE_BATCH`.
    pending: Vec<Path>,
    report: Report,
}

impl Sweep<'_> {
    /// One listed group against the table: the rows before its prefix
    /// have nothing listed, its own row has its objects, and no row at
    /// all makes its objects orphans.
    async fn merge(&mut self, rows: &mut Rows<'_>, group: Group) -> Result<(), AuditError> {
        if self
            .last
            .is_some_and(|last| key::order(group.id) <= key::order(last))
        {
            return Err(AuditError::Unsorted(group.id));
        }
        self.last = Some(group.id);
        self.listed_at = group.listed_at;
        while rows
            .peek()
            .await?
            .is_some_and(|row| key::order(row.id) < key::order(group.id))
        {
            let row = rows.next().await?.expect("peeked");
            self.check(&row, &[]).await;
        }
        if rows.peek().await?.is_some_and(|row| row.id == group.id) {
            let row = rows.next().await?.expect("peeked");
            self.check(&row, &group.objects).await;
        } else {
            // Not among the rows that require objects: a tombstone, a
            // blob awaiting its purge, a `created` one with nothing yet,
            // or no row at all. The lookup also covers a row that appeared
            // after its page was read.
            match find(self.state.pool(), group.id).await? {
                Some(row) => self.check(&row, &group.objects).await,
                None => self.unknown(&group).await,
            }
        }
        self.after = group.objects.last().map(|o| o.location.clone());
        self.cursor = group.id;
        self.save(false).await;
        Ok(())
    }

    /// Save the progress to the job, every `CHECKPOINT_EVERY` unless
    /// `now`. A save is progress made, and gives the job its attempts
    /// back: the attempts guard against failing without getting anywhere,
    /// and a restart or a retry should not cost a day-long sweep its
    /// life. Not saving is logged, as a lost lease is by the heartbeat;
    /// the sweep goes on. The deletes pending go first, so what the save
    /// says is done is done.
    async fn save(&mut self, now: bool) {
        let Some(checkpoint) = &self.checkpoint else {
            return;
        };
        if !now && checkpoint.saved.elapsed() < CHECKPOINT_EVERY {
            return;
        }
        self.delete_pending().await;
        let checkpoint = self.checkpoint.as_mut().expect("checked above");
        checkpoint.saved = Instant::now();
        let progress = Progress {
            after: self.after.as_ref().map(ToString::to_string),
            cursor: self.cursor,
            report: self.report,
        };
        let result = sqlx::query(
            "UPDATE jobs SET payload = $2, attempts = 1 \
             WHERE id = $1 AND locked_by = $3 AND failed_at IS NULL",
        )
        .bind(checkpoint.job)
        .bind(serde_json::to_value(&progress).expect("a progress serializes"))
        .bind(&checkpoint.worker)
        .execute(self.state.pool())
        .await;
        match result {
            Ok(done) if done.rows_affected() == 1 => {}
            Ok(_) => tracing::warn!("audit progress not saved: the job was reclaimed or removed"),
            Err(err) => tracing::warn!(error = %err, "could not save the audit's progress"),
        }
    }

    /// One blob against the objects under its prefix.
    async fn check(&mut self, row: &Row, objects: &[ObjectMeta]) {
        self.report.blobs += 1;
        let state = blob::state_at(row.state, row.expires_at, self.listed_at);
        if row.purged_at.is_some() {
            // A tombstone: the purge took everything, so anything left is
            // an orphan.
            for object in objects {
                self.orphan(object, "the blob was purged").await;
            }
            return;
        }
        if state.is_terminal() {
            // Awaiting its purge, which takes the whole prefix: nothing to
            // require and nothing to delete.
            return;
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

        let prefix = format!("{}/", key::prefix(row.id));
        let mut seen: HashMap<&str, &ObjectMeta> = objects
            .iter()
            .filter_map(|o| Some((o.location.as_ref().strip_prefix(prefix.as_str())?, o)))
            .collect();
        let layout = row.layout();

        // A representation is rendered from the original: with the
        // original gone or damaged, a render cannot repair it, and its
        // job would only fail its attempts and replace the stamp with its
        // own error.
        let mut lost = false;
        if let Some(key) = &row.storage_key {
            let name = key.strip_prefix(prefix.as_str()).unwrap_or(key);
            match (seen.remove(name), settled) {
                (None, true) => {
                    lost = true;
                    self.stamp(
                        row,
                        Finding::MissingOriginal,
                        "the original is missing from the store".to_owned(),
                    )
                    .await;
                }
                (Some(object), true) => {
                    let want = layout
                        .zip(row.size)
                        .map(|(layout, size)| layout.ciphertext_len(size as u64));
                    match want {
                        Some(want) if want != object.size && !self.young(object) => {
                            lost = true;
                            self.stamp(
                                row,
                                Finding::DamagedOriginal,
                                format!(
                                    "the original is {} bytes, not the {want} its size calls for",
                                    object.size
                                ),
                            )
                            .await;
                        }
                        _ => {
                            if self.sampled(row) {
                                lost = !self.deep(row).await;
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
                            lost,
                        )
                        .await;
                    }
                }
                (None, RepresentationState::Ready) => {
                    if state == BlobState::Ready && settled {
                        self.rerender(row, kind, "missing from the store", lost)
                            .await;
                    }
                }
                // A render is going to replace it.
                (Some(_), RepresentationState::Pending) => {}
                (Some(object), _) => {
                    self.orphan(object, &format!("the row says the {kind} is {rep_state}"))
                        .await;
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
            self.orphan(object, "the row does not account for it").await;
        }
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

    /// Whether this original is in the deep sample: one id in `n`, by the
    /// random low bits of the id, so the sample is the same from one run
    /// to the next and `--deep --sample N` can be run again to confirm.
    fn sampled(&self, row: &Row) -> bool {
        match self.options.deep {
            None => false,
            Some(n) => (row.id.as_u128() as u64).is_multiple_of(u64::from(n.get())),
        }
    }

    /// Decrypt the original and compare its SHA-256 with the row's. What
    /// the row or the object is at fault for is a finding; a store that
    /// cannot be read is an error, the original is left for the next run.
    /// Returns whether the original may still be intact.
    async fn deep(&mut self, row: &Row) -> bool {
        self.report.deep += 1;
        match read_back(self.state, row).await {
            Ok(digest) if row.checksum_sha256.as_deref() == Some(&digest[..]) => true,
            Ok(digest) => {
                self.stamp(
                    row,
                    Finding::DamagedOriginal,
                    format!(
                        "the original decrypts to a file with SHA-256 {}, not the one recorded",
                        hex(&digest)
                    ),
                )
                .await;
                false
            }
            Err(ReadError::Store(err)) if !matches!(err, object_store::Error::NotFound { .. }) => {
                self.failed(row.id, "reading the original back", &err);
                true
            }
            Err(err) => {
                self.stamp(
                    row,
                    Finding::DamagedOriginal,
                    format!("the original does not read back: {err}"),
                )
                .await;
                false
            }
        }
    }

    /// The audit could not act on what it found: logged, counted, and the
    /// next run finds it again.
    fn failed(&mut self, blob: Uuid, doing: &str, err: &dyn fmt::Display) {
        self.report.errors += 1;
        tracing::error!(%blob, error = %err, "audit: {doing} failed");
    }

    /// Record a finding on the original: the log line, the count, and the
    /// row's `failed_at` and `last_error` unless a verdict is on it
    /// already, or the row moved on since it was read. Nothing clears
    /// the stamp but a retry, which a blob with its original gone has
    /// nothing to retry; the first one stands.
    async fn stamp(&mut self, row: &Row, finding: Finding, what: String) {
        self.report.add(finding);
        tracing::warn!(blob = %row.id, finding = %finding, "audit: {what}");
        let result = sqlx::query(
            "UPDATE blobs SET failed_at = now(), last_error = $2 \
             WHERE id = $1 AND failed_at IS NULL AND state = $3 AND purged_at IS NULL",
        )
        .bind(row.id)
        .bind(format!("{STAMP}{what}"))
        .bind(row.state.as_str())
        .execute(self.state.pool())
        .await;
        if let Err(err) = result {
            self.failed(row.id, "stamping the blob", &err);
        }
    }

    /// The representation is not what the row says: back to `pending`
    /// and rendered again, as the read path does when it finds the object
    /// gone. Reported only when the original is `lost` too: there is
    /// nothing to render it from.
    async fn rerender(&mut self, row: &Row, kind: Representation, what: &str, lost: bool) {
        self.report.add(Finding::MissingRepresentation);
        if lost {
            tracing::warn!(blob = %row.id, finding = %Finding::MissingRepresentation, "audit: the {kind} is {what}, and the original is lost");
            return;
        }
        tracing::warn!(blob = %row.id, finding = %Finding::MissingRepresentation, "audit: the {kind} is {what}, rendering it again");
        match repo::request_representation(self.state.pool(), row.id, kind).await {
            Ok(_) => self.state.heads().invalidate(row.id),
            Err(err) => self.failed(row.id, "requesting the render", &err),
        }
    }

    fn missing_source(&mut self, row: &Row, n: u32, what: &str) {
        self.report.add(Finding::MissingSource);
        tracing::warn!(blob = %row.id, finding = %Finding::MissingSource, "audit: source part {n} {what}");
    }

    fn foreign(&mut self, object: &ObjectMeta) {
        self.report.add(Finding::Foreign);
        tracing::warn!(key = %object.location, size = object.size, finding = %Finding::Foreign, "audit: object outside the layout");
    }

    /// An object under a known prefix that its row does not account for:
    /// reported, and under `--fix` deleted, a batch at a time. One
    /// younger than `upload.stale_after` is left for the next run: an
    /// upload or a render that has written its object and not yet
    /// updated its row looks the same.
    async fn orphan(&mut self, object: &ObjectMeta, why: &str) {
        if self.young(object) {
            self.report.young += 1;
            return;
        }
        self.report.add(Finding::Orphan);
        if self.options.fix {
            tracing::warn!(key = %object.location, size = object.size, finding = %Finding::Orphan, "audit: orphan object, {why}, deleting it");
            self.delete(object).await;
        } else {
            tracing::warn!(key = %object.location, size = object.size, modified = %object.last_modified, finding = %Finding::Orphan, "audit: orphan object, {why}");
        }
    }

    /// The objects of a prefix no row has: each reported, the prefix
    /// added to the span of such prefixes, and deleted only under
    /// `--delete-unknown`. The young ones are left for the next run as
    /// orphans are, and a prefix with nothing older is not counted.
    async fn unknown(&mut self, group: &Group) {
        let mut found = false;
        for object in &group.objects {
            if self.young(object) {
                self.report.young += 1;
                continue;
            }
            found = true;
            self.report.add(Finding::Unknown);
            if self.options.delete_unknown {
                tracing::warn!(key = %object.location, size = object.size, finding = %Finding::Unknown, "audit: no row has this id, deleting the object");
                self.delete(object).await;
            } else {
                tracing::warn!(key = %object.location, size = object.size, modified = %object.last_modified, finding = %Finding::Unknown, "audit: no row has this id");
            }
        }
        if found {
            let span = self.report.unknown.get_or_insert(Unknown {
                prefixes: 0,
                first: group.id,
                last: group.id,
            });
            span.prefixes += 1;
            span.first = span.first.min(group.id);
            span.last = span.last.max(group.id);
        }
    }

    /// Queue an object for deletion, and delete the batch once full.
    async fn delete(&mut self, object: &ObjectMeta) {
        self.pending.push(object.location.clone());
        if self.pending.len() >= DELETE_BATCH {
            self.delete_pending().await;
        }
    }

    /// Delete the objects pending. One already gone is deleted; one the
    /// store would not delete is an error, and the next run finds it
    /// again.
    async fn delete_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.pending);
        let locations = stream::iter(paths.into_iter().map(Ok)).boxed();
        let mut results = self.state.store().delete_stream(locations);
        while let Some(result) = results.next().await {
            match result {
                Ok(_) | Err(object_store::Error::NotFound { .. }) => self.report.deleted += 1,
                Err(err) => {
                    self.report.errors += 1;
                    tracing::error!(error = %err, "audit: deleting an object failed");
                }
            }
        }
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
        assert_eq!(report.mismatches(), 2);
        let text = report.to_string();
        assert!(text.contains("orphan objects:          2"), "{text}");
        assert!(!text.contains("under"), "no unknown prefixes: {text}");
    }

    #[test]
    fn the_unknown_prefixes_are_reported_as_a_span_of_creation_times() {
        let first: Uuid = "01999e6c-0000-7000-8000-000000000000".parse().unwrap();
        let last = Uuid::now_v7();
        let mut report = Report::default();
        report.add(Finding::Unknown);
        report.unknown = Some(Unknown {
            prefixes: 2,
            first,
            last,
        });
        let text = report.to_string();
        assert!(text.contains("objects with no row:     1"), "{text}");
        let created = created_at(last).unwrap();
        let line = format!(
            "  under 2 prefixes, blobs created between 2025-10-01T06:18:22Z and {}",
            created.strftime("%FT%TZ")
        );
        assert!(text.contains(&line), "{text}");

        // An id without a time in it is shown as is.
        let v4: Uuid = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        let span = Unknown {
            prefixes: 1,
            first: v4,
            last: v4,
        };
        assert_eq!(span.to_string(), format!("1 prefixes, ids {v4} to {v4}"));
    }
}
