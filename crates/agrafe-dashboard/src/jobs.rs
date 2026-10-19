//! The queue: the census at the top, the listing under it with a tab per
//! state, the detail of one job, and the retry and discard actions.

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::Redirect;
use axum::routing::{get, post};
use futures::TryFutureExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::jobs::{self, Action, Count, Job, JobKind, JobState, ListParams, Priority};

use crate::error::DashboardError;
use crate::filters;
use crate::layout::{
    Base, Id, Layout, Notice, Opt, Query, ShowQuery, empty_to_none, href, options,
};
use crate::origin::SameOrigin;

pub fn routes() -> Router<Agrafe> {
    Router::new()
        .route("/jobs", get(list))
        .route("/jobs/{id}", get(show))
        .route("/jobs/{id}/retry", post(retry))
        .route("/jobs/{id}/discard", post(discard))
}

/// The filters of the listing, the fields of `jobs::ListParams` plus the
/// page's own: the URL is the state, so a filtered view is a link.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobsQuery {
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub kind: Option<JobKind>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub state: Option<JobState>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub priority: Option<Priority>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub blob_id: Option<Uuid>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor: Option<i64>,
    /// Reload the page every so many seconds, like a live poll.
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub refresh: Option<u32>,
    /// What an action that redirected here has to say; the links the
    /// page builds drop it.
    #[serde(
        default,
        deserialize_with = "Notice::lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub notice: Option<Notice>,
}

impl JobsQuery {
    fn params(&self) -> ListParams {
        ListParams {
            kind: self.kind,
            state: self.state,
            priority: self.priority,
            blob_id: self.blob_id,
            cursor: self.cursor,
            limit: jobs::DEFAULT_LIMIT,
        }
    }

    /// The same filters on another state, from the first page.
    fn tab(&self, base: &str, state: Option<JobState>) -> String {
        let q = JobsQuery {
            state,
            cursor: None,
            notice: None,
            ..self.clone()
        };
        href(base, "/jobs", &q)
    }

    fn next(&self, base: &str, cursor: i64) -> String {
        let q = JobsQuery {
            cursor: Some(cursor),
            notice: None,
            ..self.clone()
        };
        href(base, "/jobs", &q)
    }
}

/// A job with the state it is in now, which the row alone does not say.
#[derive(Debug, Clone)]
pub struct JobView {
    pub job: Job,
    pub state: JobState,
}

impl JobView {
    pub fn new(agrafe: &Agrafe, job: Job) -> Self {
        let state = agrafe.job_state(&job);
        Self { job, state }
    }

    pub fn many(agrafe: &Agrafe, jobs: Vec<Job>) -> Vec<Self> {
        jobs.into_iter().map(|j| Self::new(agrafe, j)).collect()
    }
}

/// A state tab over the listing: its total across the queue, whether it
/// is the one shown.
#[derive(Debug, Clone)]
pub struct Tab {
    pub label: String,
    pub href: String,
    pub count: i64,
    pub active: bool,
}

/// One line of the census: a kind and a priority with something in the
/// queue, how much of it in each state, and the wait of the oldest due.
#[derive(Debug, Clone, Default)]
pub struct CensusRow {
    pub kind: String,
    pub priority: String,
    pub due: i64,
    pub scheduled: i64,
    pub running: i64,
    pub dead: i64,
    pub oldest_due: Option<f64>,
}

/// The census as the header shows it: a total per state, then the rows
/// in the order of the kinds and priorities.
#[derive(Debug, Clone, Default)]
pub struct Census {
    pub totals: Vec<(JobState, i64)>,
    pub rows: Vec<CensusRow>,
}

impl Census {
    pub fn from_counts(counts: &[Count]) -> Self {
        let totals = JobState::ALL
            .iter()
            .map(|state| {
                let total = counts
                    .iter()
                    .filter(|c| c.state == *state)
                    .map(|c| c.count)
                    .sum();
                (*state, total)
            })
            .collect();
        let mut rows = Vec::new();
        for kind in JobKind::ALL {
            for priority in Priority::ALL {
                let mut row = CensusRow {
                    kind: kind.to_string(),
                    priority: priority.to_string(),
                    ..Default::default()
                };
                let mut any = false;
                for c in counts
                    .iter()
                    .filter(|c| c.kind == *kind && c.priority == *priority)
                {
                    any = true;
                    match c.state {
                        JobState::Due => {
                            row.due = c.count;
                            row.oldest_due = c.oldest_seconds;
                        }
                        JobState::Scheduled => row.scheduled = c.count,
                        JobState::Running => row.running = c.count,
                        JobState::Dead => row.dead = c.count,
                    }
                }
                if any {
                    rows.push(row);
                }
            }
        }
        Self { totals, rows }
    }

    pub fn total(&self, state: JobState) -> i64 {
        self.totals
            .iter()
            .find(|(s, _)| *s == state)
            .map_or(0, |(_, n)| *n)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "jobs.html")]
pub struct JobsPage {
    pub layout: Layout,
    pub query: JobsQuery,
    pub census: Census,
    pub tabs: Vec<Tab>,
    pub kinds: Vec<Opt>,
    pub priorities: Vec<Opt>,
    pub jobs: Vec<JobView>,
    pub next: Option<String>,
}

async fn list(
    State(agrafe): State<Agrafe>,
    base: Base,
    Query(query): Query<JobsQuery>,
) -> Result<JobsPage, DashboardError> {
    let params = query.params();
    let (counts, page) = futures::try_join!(
        agrafe.job_counts().map_err(DashboardError::from),
        agrafe.list_jobs(&params).map_err(DashboardError::from),
    )?;
    let census = Census::from_counts(&counts);
    let all = Tab {
        label: "all".to_string(),
        href: query.tab(&base.0, None),
        count: census.totals.iter().map(|(_, n)| n).sum(),
        active: query.state.is_none(),
    };
    let tabs = std::iter::once(all)
        .chain(JobState::ALL.iter().map(|state| Tab {
            label: state.to_string(),
            href: query.tab(&base.0, Some(*state)),
            count: census.total(*state),
            active: query.state == Some(*state),
        }))
        .collect();
    let next = page.next_cursor.map(|c| query.next(&base.0, c));
    Ok(JobsPage {
        layout: Layout::new(&base, "jobs")
            .notice(query.notice)
            .refresh(query.refresh),
        kinds: options(JobKind::ALL, query.kind),
        priorities: options(Priority::ALL, query.priority),
        query,
        census,
        tabs,
        jobs: JobView::many(&agrafe, page.items),
        next,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "job.html")]
pub struct JobPage {
    pub layout: Layout,
    pub view: JobView,
    pub payload: String,
    /// The buttons show: actions are on and the job is dead.
    pub can_act: bool,
}

async fn show(
    State(agrafe): State<Agrafe>,
    base: Base,
    Id(id): Id<i64>,
    Query(q): Query<ShowQuery>,
) -> Result<JobPage, DashboardError> {
    let job = agrafe.find_job(id).await?.ok_or(DashboardError::NotFound)?;
    let view = JobView::new(&agrafe, job);
    Ok(JobPage {
        layout: Layout::new(&base, "jobs").notice(q.notice),
        payload: serde_json::to_string_pretty(&view.job.payload).unwrap_or_default(),
        can_act: agrafe.config().dashboard.actions && view.state == JobState::Dead,
        view,
    })
}

/// On every action route, after the origin check: `404` unless the
/// configuration shows the buttons, so a read-only dashboard has no action
/// to be made to take.
pub struct ActionsEnabled;

impl FromRequestParts<Agrafe> for ActionsEnabled {
    type Rejection = DashboardError;

    async fn from_request_parts(_: &mut Parts, agrafe: &Agrafe) -> Result<Self, DashboardError> {
        if agrafe.config().dashboard.actions {
            Ok(ActionsEnabled)
        } else {
            Err(DashboardError::NotFound)
        }
    }
}

async fn retry(
    State(agrafe): State<Agrafe>,
    base: Base,
    _: SameOrigin,
    _: ActionsEnabled,
    Id(id): Id<i64>,
) -> Result<Redirect, DashboardError> {
    let notice = match agrafe.retry_job(id).await? {
        Action::Done(_) => Notice::JobRetried,
        Action::NotDead(_) => Notice::JobNotDead,
        Action::Duplicate(_) => Notice::JobDuplicate,
        Action::Missing => return Err(DashboardError::NotFound),
    };
    Ok(back_to_job(&base, id, notice))
}

async fn discard(
    State(agrafe): State<Agrafe>,
    base: Base,
    _: SameOrigin,
    _: ActionsEnabled,
    Id(id): Id<i64>,
) -> Result<Redirect, DashboardError> {
    match agrafe.discard_job(id).await? {
        Action::Done(_) => {
            let q = JobsQuery {
                state: Some(JobState::Dead),
                notice: Some(Notice::JobDiscarded),
                ..Default::default()
            };
            Ok(Redirect::to(&href(&base.0, "/jobs", &q)))
        }
        Action::NotDead(_) | Action::Duplicate(_) => Ok(back_to_job(&base, id, Notice::JobNotDead)),
        Action::Missing => Err(DashboardError::NotFound),
    }
}

fn back_to_job(base: &Base, id: i64, notice: Notice) -> Redirect {
    let q = ShowQuery {
        notice: Some(notice),
    };
    Redirect::to(&href(&base.0, &format!("/jobs/{id}"), &q))
}
