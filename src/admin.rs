use axum::{
    body::Body,
    extract::{FromRequestParts, State},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::storage::StorageManager;
use crate::telegram::TelegramManager;

const ADMIN_HTML: &[u8] = include_bytes!("../admin.html");

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub storage: StorageManager,
    pub telegram: TelegramManager,
}

pub struct AdminAuth;

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cookie_header = parts
            .headers
            .get(header::COOKIE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        for item in cookie_header.split(';') {
            let item = item.trim();
            if let Some(token) = item.strip_prefix("rsdlp_session=") {
                if state.db.validate_session(token).unwrap_or(false) {
                    return Ok(AdminAuth);
                }
            }
        }

        Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        ))
    }
}

pub async fn admin_html_handler() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(ADMIN_HTML))
        .unwrap()
}

#[derive(Deserialize)]
pub struct LoginReq {
    pub password: String,
}

pub async fn admin_login_handler(
    State(state): State<AppState>,
    Json(payload): Json<LoginReq>,
) -> Response {
    match state.db.verify_admin_password(&payload.password).await {
        Ok(true) => {
            match state.db.create_session() {
                Ok(token) => {
                    let cookie = format!(
                        "rsdlp_session={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age=2592000"
                    );
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::SET_COOKIE, cookie)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"success":true}"#))
                        .unwrap()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e })),
                )
                    .into_response(),
            }
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Invalid admin password" })),
        )
            .into_response(),
    }
}

pub async fn admin_logout_handler(
    State(state): State<AppState>,
    parts: Parts,
) -> Response {
    let cookie_header = parts
        .headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    for item in cookie_header.split(';') {
        let item = item.trim();
        if let Some(token) = item.strip_prefix("rsdlp_session=") {
            let _ = state.db.delete_session(token);
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::SET_COOKIE,
            "rsdlp_session=; HttpOnly; Path=/; Max-Age=0",
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"success":true}"#))
        .unwrap()
}

#[derive(Serialize)]
pub struct AdminStatusResp {
    pub storage: Option<crate::storage::DiskSpace>,
    pub telegram: crate::telegram::TelegramStatus,
}

pub async fn admin_status_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
) -> Json<AdminStatusResp> {
    let storage = state.storage.query_disk_space();
    let telegram = state.telegram.get_status().await;
    Json(AdminStatusResp { storage, telegram })
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    pub old_password: String,
    pub new_password: String,
}

pub async fn admin_change_password_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
    Json(payload): Json<ChangePasswordReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let is_valid = state
        .db
        .verify_admin_password(&payload.old_password)
        .await
        .unwrap_or(false);
    if !is_valid {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Incorrect current password" })),
        ));
    }

    if payload.new_password.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "New password cannot be empty" })),
        ));
    }

    state
        .db
        .set_admin_password(&payload.new_password)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            )
        })?;

    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn admin_prune_temp_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let pruned = state.storage.prune_stale_files();
    Json(serde_json::json!({ "pruned": pruned }))
}

#[derive(Serialize)]
pub struct TgConfigResp {
    pub api_id: Option<i32>,
    pub api_hash: Option<String>,
    pub bot_token: Option<String>,
}

pub async fn admin_tg_config_get_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
) -> Json<TgConfigResp> {
    let cfg = state.telegram.get_config().await;
    Json(TgConfigResp {
        api_id: cfg.as_ref().map(|c| c.api_id),
        api_hash: cfg.as_ref().map(|c| c.api_hash.clone()),
        bot_token: cfg.map(|c| c.bot_token),
    })
}

#[derive(Deserialize)]
pub struct TgConfigReq {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: String,
}

pub async fn admin_tg_config_set_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
    Json(payload): Json<TgConfigReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if payload.api_id <= 0 || payload.api_hash.trim().is_empty() || payload.bot_token.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "API ID, API Hash, and Bot Token are required" })),
        ));
    }

    state
        .db
        .set_config("tg_api_id", &payload.api_id.to_string())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))))?;
    state
        .db
        .set_config("tg_api_hash", &payload.api_hash)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))))?;
    state
        .db
        .set_config("tg_bot_token", &payload.bot_token)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))))?;

    // Auto-start bot on credential save
    if let Err(e) = state.telegram.start_bot().await {
        tracing::warn!("[telegram] Failed to start bot after saving config: {e}");
    }

    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn admin_tg_start_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state
        .telegram
        .start_bot()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))))?;

    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn admin_tg_stop_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    state.telegram.stop_bot().await;
    Json(serde_json::json!({ "success": true }))
}

pub async fn admin_tg_disconnect_handler(
    _auth: AdminAuth,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state
        .telegram
        .disconnect()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))))?;

    Ok(Json(serde_json::json!({ "success": true })))
}
