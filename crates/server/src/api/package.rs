use crate::services::core::{CoreService, PackageRecordInfo, RecordState};
use crate::AnyError;
use anyhow::{Error, Result};
use axum::{
    debug_handler,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use std::str::FromStr;
use std::sync::Arc;
use warg_api::{
    content::ContentSourceKind,
    package::{PendingRecordResponse, PublishRequest, RecordResponse},
};
use warg_crypto::hash::{DynHash, Sha256};
use warg_protocol::registry::{LogId, RecordId};

#[derive(Clone)]
pub struct Config {
    core_service: Arc<CoreService>,
    base_url: String,
}

impl Config {
    pub fn new(core_service: Arc<CoreService>, base_url: String) -> Self {
        Self {
            core_service,
            base_url,
        }
    }

    pub fn build_router(self) -> Router {
        Router::new()
            .route("/", post(publish))
            .route("/:package_id/records/:record_id", get(get_record))
            .route("/:package_id/pending/:record_id", get(get_pending_record))
            .with_state(self)
    }
}

fn record_url(package_id: &LogId, record_id: &RecordId) -> String {
    format!("/package/{package_id}/records/{record_id}")
}

fn pending_record_url(package_id: &LogId, record_id: &RecordId) -> String {
    format!("/package/{package_id}/pending/{record_id}")
}

fn create_pending_response(
    package_id: &LogId,
    record_id: &RecordId,
    state: RecordState,
) -> Result<PendingRecordResponse, AnyError> {
    let response = match state {
        RecordState::Unknown => return Err(Error::msg("Internal error").into()),
        RecordState::Processing => PendingRecordResponse::Processing {
            status_url: pending_record_url(package_id, record_id),
        },
        RecordState::Published { .. } => PendingRecordResponse::Published {
            record_url: record_url(package_id, record_id),
        },
        RecordState::Rejected { reason } => PendingRecordResponse::Rejected { reason },
    };
    Ok(response)
}

#[debug_handler]
pub(crate) async fn publish(
    State(config): State<Config>,
    Json(body): Json<PublishRequest>,
) -> Result<impl IntoResponse, AnyError> {
    let record = Arc::new(body.record.try_into()?);
    let record_id = RecordId::package_record::<Sha256>(&record);

    for source in body.content_sources.iter() {
        match &source.kind {
            ContentSourceKind::HttpAnonymous { url } => {
                if url.starts_with(&config.base_url) {
                    let response = Client::builder().build()?.head(url).send().await?;
                    if !response.status().is_success() {
                        return Err(Error::msg("Unable to validate content is present").into());
                    }
                } else {
                    return Err(Error::msg("URL not from current host").into());
                }
            }
        }
    }

    let package_id = LogId::package_log::<Sha256>(&body.name);

    let state = config
        .core_service
        .submit_package_record(body.name, record, body.content_sources)
        .await;

    let response = create_pending_response(&package_id, &record_id, state)?;
    Ok((StatusCode::OK, Json(response)))
}

#[debug_handler]
pub(crate) async fn get_record(
    State(config): State<Config>,
    Path((log_id, record_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AnyError> {
    let log_id: LogId = DynHash::from_str(&log_id)?.into();
    let record_id: RecordId = DynHash::from_str(&record_id)?.into();

    let info = config
        .core_service
        .get_package_record_info(log_id, record_id)
        .await;
    match info {
        Some(PackageRecordInfo {
            record,
            content_sources,
            state: RecordState::Published { checkpoint },
        }) => {
            let response = RecordResponse {
                record: record.as_ref().clone().into(),
                content_sources,
                checkpoint,
            };
            Ok((StatusCode::OK, Json(response)))
        }
        _ => Err(Error::msg("Not found").into()), // todo: improve to 404
    }
}

#[debug_handler]
pub(crate) async fn get_pending_record(
    State(config): State<Config>,
    Path((package_id, record_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AnyError> {
    let package_id: LogId = DynHash::from_str(&package_id)?.into();
    let record_id: RecordId = DynHash::from_str(&record_id)?.into();

    let status = config
        .core_service
        .get_package_record_status(package_id.clone(), record_id.clone())
        .await;

    let response = create_pending_response(&package_id, &record_id, status)?;
    Ok((StatusCode::OK, Json(response)))
}