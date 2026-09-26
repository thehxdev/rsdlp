use axum::{
    Json, Router,
    body::Body,
    extract::{Form, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

mod ytdlp;
mod telegram;
mod db;
mod storage;
mod admin;

const INDEX_HTML: &[u8] = include_bytes!("../index.html");

#[derive(Deserialize)]
struct DownloadForm {
    url: String,
    // Strings, not u64: an HTML form submits `res=` (empty) when omitted,
    // which serde_urlencoded would reject as an invalid u64.
    res: Option<String>,
    abr: Option<String>,
}

/// Treat an absent and an empty value alike,
/// and reject anything that isn't a plain number with a 400.
fn parse_quality(name: &str, raw: Option<String>) -> Result<Option<u64>, StatusCode> {
    match raw {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => raw
            .trim()
            .parse()
            .map(Some)
            .map_err(|e| {
                tracing::warn!("invalid {name}: {e}");
                StatusCode::BAD_REQUEST
            }),
    }
}

#[derive(Deserialize)]
struct QualitiesForm {
    url: String,
}

fn parse_dotenv_content(content: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim().to_string();
            let val = val.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            vars.push((key, val));
        }
    }
    vars
}

/// Load key-value pairs from `.env` file into process environment if not already set.
fn load_dotenv() {
    let Ok(content) = std::fs::read_to_string(".env") else {
        return;
    };
    for (key, val) in parse_dotenv_content(&content) {
        if std::env::var(&key).is_err() {
            // ponytail: basic .env parser; upgrade to dotenvy if complex multiline/escaping needed
            unsafe {
                std::env::set_var(&key, val);
            }
        }
    }
}

/// Determine active log filter: environment variable has priority over CLI flag.
fn determine_log_filter(args: &[String]) -> String {
    // 1. Environment variable has highest priority
    if let Ok(env_val) = std::env::var("RSDLP_LOG").or_else(|_| std::env::var("RUST_LOG")) {
        let trimmed = env_val.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    // 2. Command-line flag
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--log-level" || arg == "--log" || arg == "-l" {
            if let Some(val) = iter.next() {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        } else if let Some(val) = arg.strip_prefix("--log-level=").or_else(|| arg.strip_prefix("--log=")) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    // 3. Default level
    "info".to_string()
}

fn init_logging(filter_directive: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(filter_directive)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    load_dotenv();

    let args: Vec<String> = std::env::args().collect();
    let log_filter = determine_log_filter(&args);
    init_logging(&log_filter);
    tracing::info!("rsdlp starting (log_filter={})", log_filter);

    let data_dir = std::env::var("RSDLP_DATA_DIR")
        .unwrap_or_else(|_| ".".to_string());
    let db_path = format!("{}/rsdlp.db", data_dir);
    let session_path = std::env::var("RSDLP_TG_SESSION_FILE")
        .unwrap_or_else(|_| format!("{}/rsdlp.session", data_dir));
    let staging_dir = std::path::PathBuf::from(format!("{}/downloads", data_dir));

    let db = db::Database::open(&db_path)?;
    let storage_manager = storage::StorageManager::new(staging_dir.clone());
    let pruned = storage_manager.prune_stale_files();
    if pruned > 0 {
        tracing::info!("pruned {pruned} stale download staging file(s) on startup");
    }

    let telegram_manager = telegram::TelegramManager::new(db.clone(), session_path, staging_dir);

    let tm = telegram_manager.clone();
    tokio::spawn(async move {
        tm.init_from_db().await;
    });

    let app_state = admin::AppState {
        db,
        storage: storage_manager,
        telegram: telegram_manager,
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/download", post(download_handler))
        .route("/qualities", post(qualities_handler))
        .route("/admin", get(admin::admin_html_handler))
        .route("/api/admin/login", post(admin::admin_login_handler))
        .route("/api/admin/logout", post(admin::admin_logout_handler))
        .route("/api/admin/status", get(admin::admin_status_handler))
        .route("/api/admin/change-password", post(admin::admin_change_password_handler))
        .route("/api/admin/prune-temp", post(admin::admin_prune_temp_handler))
        .route(
            "/api/admin/telegram/config",
            get(admin::admin_tg_config_get_handler).post(admin::admin_tg_config_set_handler),
        )
        .route("/api/admin/telegram/start", post(admin::admin_tg_start_handler))
        .route("/api/admin/telegram/stop", post(admin::admin_tg_stop_handler))
        .route("/api/admin/telegram/disconnect", post(admin::admin_tg_disconnect_handler))
        .route(
            "/api/admin/blacklist",
            get(admin::admin_blacklist_get_handler).post(admin::admin_blacklist_add_handler),
        )
        .route(
            "/api/admin/blacklist/delete",
            post(admin::admin_blacklist_remove_handler),
        )
        .with_state(app_state);

    let bind_address = std::env::var("RSDLP_BIND_ADDRESS")
        .or_else(|_| std::env::var("BIND_ADDRESS"))
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_address).await?;
    tracing::info!("listening on {bind_address}");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn index_handler() -> Response {
    tracing::debug!("GET / index");
    Response::builder()
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Body::from(INDEX_HTML))
        .unwrap()
}

pub fn blacklisted_provider_notice(domain: &str) -> String {
    format!(
        "This media provider ('{domain}') is currently disabled because processing its streams is a heavy operation that requires intensive CPU and server resources."
    )
}

async fn qualities_handler(
    State(state): State<admin::AppState>,
    Form(form): Form<QualitiesForm>,
) -> Response {
    tracing::info!("qualities requested for url={}", form.url);
    if let Ok(Some(domain)) = state.db.check_url_blacklisted(&form.url) {
        let msg = blacklisted_provider_notice(&domain);
        tracing::warn!("blocked blacklisted provider url: {} ({domain})", form.url);
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": msg,
                "blacklisted": true,
                "domain": domain
            })),
        )
            .into_response();
    }

    let ytdlp = ytdlp::Ytdlp::new(&form.url, None);

    let info = match ytdlp.get_info().await {
        Ok(Some(info)) => info,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid media URL" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("failed to get info for qualities: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to get media info" })),
            )
                .into_response();
        }
    };

    Json(ytdlp::extract_qualities(&info)).into_response()
}

async fn download_handler(
    State(state): State<admin::AppState>,
    Form(form): Form<DownloadForm>,
) -> Response {
    if let Ok(Some(domain)) = state.db.check_url_blacklisted(&form.url) {
        let msg = blacklisted_provider_notice(&domain);
        tracing::warn!("blocked blacklisted provider download: {} ({domain})", form.url);
        return (StatusCode::FORBIDDEN, msg).into_response();
    }

    let res = match parse_quality("res", form.res) {
        Ok(v) => v,
        Err(status) => return (status, "Invalid res").into_response(),
    };
    let abr = match parse_quality("abr", form.abr) {
        Ok(v) => v,
        Err(status) => return (status, "Invalid abr").into_response(),
    };

    if res.is_none() && abr.is_none() {
        tracing::warn!("missing quality: at least one of res or abr must be specified");
        return (StatusCode::BAD_REQUEST, "Missing quality").into_response();
    }

    tracing::info!("starting download for url={}, res={:?}, abr={:?}", form.url, res, abr);

    let mut resp = Response::builder();
    let mut ytdlp = ytdlp::Ytdlp::new(&form.url, Some(ytdlp::build_sort(res, abr)));

    {
        let info = match ytdlp.get_info().await {
            Ok(Some(info)) => info,
            Ok(None) => return StatusCode::BAD_REQUEST.into_response(),
            Err(e) => {
                tracing::error!("failed to get info for download: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

        let filename = ytdlp::extract_filename(&info);
        resp = resp.header("Content-Disposition", format!("attachment; filename=\"{filename}\""));
    }

    if let Err(e) = ytdlp.start_download() {
        tracing::error!("failed to start download: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let stream = tokio_util::io::ReaderStream::new(ytdlp);
    let body = Body::from_stream(stream);

    match resp.body(body) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("failed to stream response body: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_log_filter_env_overrides_cli() {
        unsafe {
            std::env::set_var("RSDLP_LOG", "debug");
        }
        let args = vec!["rsdlp".to_string(), "--log-level".to_string(), "error".to_string()];
        assert_eq!(determine_log_filter(&args), "debug");

        unsafe {
            std::env::remove_var("RSDLP_LOG");
        }
    }

    #[test]
    fn test_determine_log_filter_cli_flag() {
        unsafe {
            std::env::remove_var("RSDLP_LOG");
            std::env::remove_var("RUST_LOG");
        }
        let args = vec!["rsdlp".to_string(), "--log-level".to_string(), "warn".to_string()];
        assert_eq!(determine_log_filter(&args), "warn");

        let args_eq = vec!["rsdlp".to_string(), "--log=trace".to_string()];
        assert_eq!(determine_log_filter(&args_eq), "trace");

        let args_short = vec!["rsdlp".to_string(), "-l".to_string(), "error".to_string()];
        assert_eq!(determine_log_filter(&args_short), "error");
    }

    #[test]
    fn test_determine_log_filter_default() {
        unsafe {
            std::env::remove_var("RSDLP_LOG");
            std::env::remove_var("RUST_LOG");
        }
        let args = vec!["rsdlp".to_string()];
        assert_eq!(determine_log_filter(&args), "info");
    }

    #[test]
    fn test_parse_dotenv_content() {
        let sample = r#"
        # Comment line
        RSDLP_BIND_ADDRESS="127.0.0.1:8080"
        RSDLP_TG_API_ID=12345
        EMPTY_VAR=
        QUOTED_VAL='single-quoted'
        "#;
        let parsed = parse_dotenv_content(sample);
        assert_eq!(
            parsed,
            vec![
                ("RSDLP_BIND_ADDRESS".to_string(), "127.0.0.1:8080".to_string()),
                ("RSDLP_TG_API_ID".to_string(), "12345".to_string()),
                ("EMPTY_VAR".to_string(), "".to_string()),
                ("QUOTED_VAL".to_string(), "single-quoted".to_string()),
            ]
        );
    }
}
