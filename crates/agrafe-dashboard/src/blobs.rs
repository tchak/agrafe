//! The blobs: the listing with the six filters of `repo::ListParams`, the
//! detail with the thumbnail, the attachments and the jobs of one blob,
//! and the retry action for a blob whose job died.

use std::collections::HashSet;

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::State;
use axum::response::Redirect;
use axum::routing::{get, post};
use futures::TryFutureExt;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::attachment::Attachment;
use agrafe::blob::repo::{self, ListParams};
use agrafe::blob::urls::encode_segment;
use agrafe::blob::{Blob, BlobState, RetryError, ScanState};
use agrafe::classify::Category;
use agrafe::jobs;

use crate::error::DashboardError;
use crate::filters;
use crate::jobs::{ActionsEnabled, JobView};
use crate::layout::{
    Base, Id, Layout, Notice, Opt, Query, ShowQuery, bool_options, empty_to_none, href, options,
};
use crate::origin::SameOrigin;

pub fn routes() -> Router<Agrafe> {
    Router::new()
        .route("/blobs", get(list))
        .route("/blobs/{id}", get(show))
        .route("/blobs/{id}/retry", post(retry))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlobsQuery {
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub content_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub category: Option<Category>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub state: Option<BlobState>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub scan_state: Option<ScanState>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub attached: Option<bool>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub failed: Option<bool>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor: Option<Uuid>,
}

impl BlobsQuery {
    fn params(&self) -> ListParams {
        ListParams {
            content_type: self.content_type.clone(),
            category: self.category,
            state: self.state,
            scan_state: self.scan_state,
            attached: self.attached,
            failed: self.failed,
            cursor: self.cursor,
            limit: repo::DEFAULT_LIMIT,
        }
    }

    fn next(&self, base: &str, cursor: Uuid) -> String {
        let q = BlobsQuery {
            cursor: Some(cursor),
            ..self.clone()
        };
        href(base, "/blobs", &q)
    }
}

/// A blob with the state it is in now and whether anything is attached to
/// it, which the row alone does not say.
#[derive(Debug, Clone)]
pub struct BlobView {
    pub blob: Blob,
    pub state: BlobState,
    pub attached: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "blobs.html")]
pub struct BlobsPage {
    pub layout: Layout,
    pub query: BlobsQuery,
    pub categories: Vec<Opt>,
    pub states: Vec<Opt>,
    pub scan_states: Vec<Opt>,
    pub attached: Vec<Opt>,
    pub failed: Vec<Opt>,
    pub blobs: Vec<BlobView>,
    pub next: Option<String>,
}

async fn list(
    State(agrafe): State<Agrafe>,
    base: Base,
    Query(query): Query<BlobsQuery>,
) -> Result<BlobsPage, DashboardError> {
    let page = agrafe.list_blobs(query.params()).await?;
    let ids: Vec<Uuid> = page.items.iter().map(|b| b.id).collect();
    let attached: HashSet<Uuid> = agrafe
        .attachments_for(&ids)
        .await?
        .into_iter()
        .map(|a| a.blob_id)
        .collect();
    let now = Timestamp::now();
    let blobs = page
        .items
        .into_iter()
        .map(|blob| BlobView {
            state: blob.state_at(now),
            attached: attached.contains(&blob.id),
            blob,
        })
        .collect();
    let next = page.next_cursor.map(|c| query.next(&base.0, c));
    Ok(BlobsPage {
        layout: Layout::new(&base, "blobs"),
        categories: options(Category::ALL, query.category),
        states: options(BlobState::ALL, query.state),
        scan_states: options(ScanState::ALL, query.scan_state),
        attached: bool_options(query.attached),
        failed: bool_options(query.failed),
        query,
        blobs,
        next,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "blob.html")]
pub struct BlobPage {
    pub layout: Layout,
    pub view: BlobView,
    /// The private thumbnail route, always `200`, on the same listener.
    pub thumbnail: String,
    pub attachments: Vec<Attachment>,
    pub jobs: Vec<JobView>,
    /// The button shows: actions are on and the job that was to finish
    /// the blob died.
    pub can_retry: bool,
}

async fn show(
    State(agrafe): State<Agrafe>,
    base: Base,
    Id(id): Id<Uuid>,
    Query(q): Query<ShowQuery>,
) -> Result<BlobPage, DashboardError> {
    let params = jobs::ListParams {
        blob_id: Some(id),
        limit: jobs::DEFAULT_LIMIT,
        ..Default::default()
    };
    let ids = [id];
    let (blob, attachments, jobs) = futures::try_join!(
        agrafe.find_blob(id).map_err(DashboardError::from),
        agrafe.attachments_for(&ids).map_err(DashboardError::from),
        agrafe.list_jobs(&params).map_err(DashboardError::from),
    )?;
    let blob = blob.ok_or(DashboardError::NotFound)?;
    let thumbnail = format!(
        "{}/blobs/{id}/thumbnail/{}",
        base.api(),
        encode_segment(&blob.filename)
    );
    Ok(BlobPage {
        layout: Layout::new(&base, "blobs").notice(q.notice),
        can_retry: agrafe.config().dashboard.actions && blob.failed_at.is_some(),
        view: BlobView {
            state: blob.state_at(Timestamp::now()),
            attached: !attachments.is_empty(),
            blob,
        },
        thumbnail,
        attachments,
        jobs: JobView::many(&agrafe, jobs.items),
    })
}

async fn retry(
    State(agrafe): State<Agrafe>,
    base: Base,
    _: SameOrigin,
    _: ActionsEnabled,
    Id(id): Id<Uuid>,
) -> Result<Redirect, DashboardError> {
    let notice = match agrafe.retry_blob(id).await {
        Ok(_) => Notice::BlobRetried,
        Err(RetryError::Terminal(_)) => Notice::BlobTerminal,
        Err(RetryError::Nothing(_)) => Notice::BlobNothing,
        Err(RetryError::Pending) => Notice::BlobPending,
        Err(RetryError::NotFound) => return Err(DashboardError::NotFound),
        Err(RetryError::Db(e)) => return Err(DashboardError::internal(e)),
    };
    let q = ShowQuery {
        notice: Some(notice),
    };
    Ok(Redirect::to(&href(&base.0, &format!("/blobs/{id}"), &q)))
}
