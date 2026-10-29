//! What every page shares: where the dashboard is mounted, the chrome
//! around the content, and the extractors that turn a bad request into a
//! [`DashboardError`] rather than axum's plain-text rejection.

use std::convert::Infallible;
use std::fmt::Display;
use std::str::FromStr;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRequestParts, OriginalUri};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::DashboardError;

/// Where the dashboard is mounted, read off the request: the part of the
/// original path the host's `nest` stripped. Empty when merged at the root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Base(pub String);

impl Base {
    /// The prefix the private API answers under, where the thumbnail route
    /// is: the dashboard is assumed to be mounted one segment below it.
    pub fn api(&self) -> &str {
        self.0.rsplit_once('/').map_or("", |(head, _)| head)
    }

    fn from_paths(original: &str, stripped: &str) -> Self {
        let base = if stripped == "/" {
            original
        } else {
            original.strip_suffix(stripped).unwrap_or("")
        };
        Self(base.trim_end_matches('/').to_string())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Base {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let stripped = parts.uri.path();
        let original = parts
            .extensions
            .get::<OriginalUri>()
            .map_or(stripped, |o| o.0.path());
        Ok(Self::from_paths(original, stripped))
    }
}

/// The shortest auto-refresh a page takes, in seconds.
pub const MIN_REFRESH: u32 = 5;

/// The chrome of a page: the mount point its links start from, the notice
/// an action left, and the auto-refresh an operator asked for.
#[derive(Debug, Clone)]
pub struct Layout {
    pub base: String,
    pub section: &'static str,
    pub notice: Option<Notice>,
    pub refresh: Option<u32>,
}

impl Layout {
    pub fn new(base: &Base, section: &'static str) -> Self {
        Self {
            base: base.0.clone(),
            section,
            notice: None,
            refresh: None,
        }
    }

    pub fn notice(mut self, notice: Option<Notice>) -> Self {
        self.notice = notice;
        self
    }

    /// Reload every so many seconds, `MIN_REFRESH` at the least: each
    /// reload of the queue page runs the census and a listing again, and a
    /// pasted `?refresh=1` is not to run them every second.
    pub fn refresh(mut self, seconds: Option<u32>) -> Self {
        self.refresh = seconds.filter(|s| *s > 0).map(|s| s.max(MIN_REFRESH));
        self
    }
}

/// What an action tells the page it redirects to, as a code in the query.
/// The page holds the sentences, so a pasted link cannot make the
/// dashboard say anything of its own; a code the page does not know is
/// ignored rather than echoed in an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Notice {
    JobRetried,
    JobDiscarded,
    JobNotDead,
    JobDuplicate,
    BlobRetried,
    BlobTerminal,
    BlobNothing,
    BlobPending,
}

impl Notice {
    pub fn message(self) -> &'static str {
        match self {
            Notice::JobRetried => "Job queued again.",
            Notice::JobDiscarded => "Job discarded.",
            Notice::JobNotDead => {
                "Nothing done: only a dead job can be retried or discarded, and this one is not dead."
            }
            Notice::JobDuplicate => {
                "Nothing to retry: a job of this kind is already pending for the blob."
            }
            Notice::BlobRetried => "Retry queued: the blob is waiting on its job again.",
            Notice::BlobTerminal => {
                "Nothing queued: the blob is expired, deleted or infected, and nothing will finish it."
            }
            Notice::BlobNothing => "Nothing queued: the blob is not waiting on a job.",
            Notice::BlobPending => "Nothing queued: a job is already pending for this blob.",
        }
    }
}

impl Notice {
    /// `deserialize_with` for a query field: an unknown code is `None`.
    pub fn lenient<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Self>, D::Error> {
        let raw = Option::<String>::deserialize(deserializer)?;
        Ok(raw.and_then(|s| serde_plain::from_str(&s).ok()))
    }
}

/// The query of a detail page, and what an action redirects to one with.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShowQuery {
    #[serde(
        default,
        deserialize_with = "Notice::lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub notice: Option<Notice>,
}

/// What a failure shows: the status and its reason as the title, the
/// message under it.
#[derive(Template, WebTemplate)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub layout: Layout,
    pub status: u16,
    pub reason: &'static str,
    pub message: String,
}

/// One `<option>` of a filter's `<select>`.
#[derive(Debug, Clone)]
pub struct Opt {
    pub value: String,
    pub selected: bool,
}

/// The options of a select over `all`, the current one marked.
pub fn options<T: Display + PartialEq + Copy>(all: &[T], current: Option<T>) -> Vec<Opt> {
    all.iter()
        .map(|v| Opt {
            value: v.to_string(),
            selected: current == Some(*v),
        })
        .collect()
}

/// The options of a select over the values a column takes, the current
/// one marked, and listed even when it is not among them: a pasted link
/// may name a value that has since gone.
pub fn options_of(values: &[String], current: Option<&str>) -> Vec<Opt> {
    let mut opts: Vec<Opt> = values
        .iter()
        .map(|v| Opt {
            value: v.clone(),
            selected: current == Some(v.as_str()),
        })
        .collect();
    if let Some(current) = current
        && !opts.iter().any(|o| o.selected)
    {
        opts.push(Opt {
            value: current.to_string(),
            selected: true,
        });
    }
    opts
}

/// `yes` / `no` as `true` / `false`.
pub fn bool_options(current: Option<bool>) -> Vec<Opt> {
    options(&[true, false], current)
}

/// A link under the mount point with `query` encoded after it, nothing
/// after the `?` when the query is empty.
pub fn href(base: &str, path: &str, query: &impl Serialize) -> String {
    match serde_urlencoded::to_string(query) {
        Ok(qs) if !qs.is_empty() => format!("{base}{path}?{qs}"),
        _ => format!("{base}{path}"),
    }
}

/// A form field left blank is a filter not set: `kind=` is no `kind`.
pub fn empty_to_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => s.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

/// `axum::extract::Query` whose rejection is a `400` page.
pub struct Query<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = DashboardError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, DashboardError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(q)) => Ok(Query(q)),
            Err(rejection) => Err(DashboardError::BadRequest(match rejection {
                QueryRejection::FailedToDeserializeQueryString(e) => e.body_text(),
                other => other.body_text(),
            })),
        }
    }
}

/// `axum::extract::Path` for one id whose rejection is a `404` page: an id
/// of the wrong shape names nothing.
pub struct Id<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Id<T> {
    type Rejection = DashboardError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, DashboardError> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(id)) => Ok(Id(id)),
            Err(PathRejection::FailedToDeserializePathParams(_)) => Err(DashboardError::NotFound),
            Err(other) => Err(DashboardError::internal(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_is_what_the_nest_stripped() {
        assert_eq!(Base::from_paths("/dashboard/jobs", "/jobs").0, "/dashboard");
        assert_eq!(Base::from_paths("/dashboard", "/").0, "/dashboard");
        assert_eq!(Base::from_paths("/dashboard/", "/").0, "/dashboard");
        assert_eq!(Base::from_paths("/jobs", "/jobs").0, "");
        assert_eq!(Base::from_paths("/", "/").0, "");
        assert_eq!(
            Base::from_paths("/internal/dashboard/blobs/x", "/blobs/x").0,
            "/internal/dashboard"
        );
    }

    #[test]
    fn the_api_is_one_segment_up() {
        assert_eq!(Base("/dashboard".into()).api(), "");
        assert_eq!(Base("/internal/dashboard".into()).api(), "/internal");
        assert_eq!(Base(String::new()).api(), "");
    }

    #[test]
    fn links_carry_the_query() {
        #[derive(Serialize)]
        struct Q {
            #[serde(skip_serializing_if = "Option::is_none")]
            a: Option<&'static str>,
        }
        assert_eq!(href("/d", "/jobs", &Q { a: None }), "/d/jobs");
        assert_eq!(href("/d", "/jobs", &Q { a: Some("x y") }), "/d/jobs?a=x+y");
    }
}
