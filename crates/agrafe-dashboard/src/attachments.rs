//! The attachments: the listing with the four filters of
//! `attachment::ListParams`, each row with the state of its blob, and the
//! detail of one.

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::attachment::{self, Attachment, ListParams, Listed};

use crate::error::DashboardError;
use crate::filters;
use crate::layout::{Base, Id, Layout, Opt, Query, empty_to_none, href, options_of};

pub fn routes() -> Router<Agrafe> {
    Router::new()
        .route("/attachments", get(list))
        .route("/attachments/{id}", get(show))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachmentsQuery {
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub record_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub record_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "empty_to_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<String>,
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
    pub cursor: Option<Uuid>,
}

impl AttachmentsQuery {
    fn params(&self) -> ListParams {
        ListParams {
            record_type: self.record_type.clone(),
            record_id: self.record_id.clone(),
            name: self.name.clone(),
            blob_id: self.blob_id,
            cursor: self.cursor,
            limit: attachment::DEFAULT_LIMIT,
        }
    }

    fn next(&self, base: &str, cursor: Uuid) -> String {
        let q = AttachmentsQuery {
            cursor: Some(cursor),
            ..self.clone()
        };
        href(base, "/attachments", &q)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "attachments.html")]
pub struct AttachmentsPage {
    pub layout: Layout,
    pub query: AttachmentsQuery,
    /// The values `record_type` and `name` take, as selects: a backend has
    /// a handful of each, and `Agrafe::attachment_facets` finds them
    /// without reading the table.
    pub record_types: Vec<Opt>,
    pub names: Vec<Opt>,
    pub attachments: Vec<Listed>,
    pub next: Option<String>,
}

async fn list(
    State(agrafe): State<Agrafe>,
    base: Base,
    Query(query): Query<AttachmentsQuery>,
) -> Result<AttachmentsPage, DashboardError> {
    let page = agrafe.list_attachments(&query.params()).await?;
    let facets = agrafe.attachment_facets().await?;
    let next = page.next_cursor.map(|c| query.next(&base.0, c));
    Ok(AttachmentsPage {
        layout: Layout::new(&base, "attachments"),
        record_types: options_of(&facets.record_types, query.record_type.as_deref()),
        names: options_of(&facets.names, query.name.as_deref()),
        query,
        attachments: page.items,
        next,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "attachment.html")]
pub struct AttachmentPage {
    pub layout: Layout,
    pub attachment: Attachment,
}

async fn show(
    State(agrafe): State<Agrafe>,
    base: Base,
    Id(id): Id<Uuid>,
) -> Result<AttachmentPage, DashboardError> {
    let attachment = agrafe
        .find_attachment(id)
        .await?
        .ok_or(DashboardError::NotFound)?;
    Ok(AttachmentPage {
        layout: Layout::new(&base, "attachments"),
        attachment,
    })
}
