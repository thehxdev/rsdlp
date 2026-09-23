use axum::{
    Json, Router,
    body::Body,
    extract::Form,
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use serde::Deserialize;

mod ytdlp;
mod telegram;

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

    if let Some(tg_config) = telegram::TelegramConfig::from_env() {
        if args.iter().any(|arg| arg == "--tg-login") {
            tracing::info!("Running Telegram interactive login...");
            let session = std::sync::Arc::new(grammers_session::storages::SqliteSession::open(&tg_config.session_file)?);
            let pool = grammers_mtsender::SenderPool::new(std::sync::Arc::clone(&session), tg_config.api_id);
            let client = grammers_client::Client::new(&pool);
            tokio::spawn(pool.runner.run());
            telegram::authorize_client(&client, &tg_config.api_hash, tg_config.bot_token.as_deref()).await?;
            tracing::info!("Login complete. Session saved to {}", tg_config.session_file);
            return Ok(());
        }

        tokio::spawn(async move {
            if let Err(e) = telegram::run_bot(tg_config).await {
                tracing::error!("[telegram] bot error: {e}");
            }
        });
    } else {
        tracing::info!("Telegram credentials not set (RSDLP_TG_API_ID / RSDLP_TG_API_HASH). Running web-only mode.");
    }

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/download", post(download_handler))
        .route("/qualities", post(qualities_handler));

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

async fn qualities_handler(
    Form(form): Form<QualitiesForm>,
) -> Result<Json<ytdlp::Qualities>, StatusCode> {
    tracing::info!("qualities requested for url={}", form.url);
    let ytdlp = ytdlp::Ytdlp::new(&form.url, None);

    let info = ytdlp.get_info().await
        .map_err(|e| {
            tracing::error!("failed to get info for qualities: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::BAD_REQUEST)?;

    Ok(Json(ytdlp::extract_qualities(&info)))
}

async fn download_handler(Form(form): Form<DownloadForm>) -> Result<Response, StatusCode> {
    let res = parse_quality("res", form.res)?;
    let abr = parse_quality("abr", form.abr)?;

    if res.is_none() && abr.is_none() {
        tracing::warn!("missing quality: at least one of res or abr must be specified");
        return Err(StatusCode::BAD_REQUEST);
    }

    tracing::info!("starting download for url={}, res={:?}, abr={:?}", form.url, res, abr);

    let mut resp = Response::builder();
    let mut ytdlp = ytdlp::Ytdlp::new(&form.url, Some(ytdlp::build_sort(res, abr)));

    {
        let info = ytdlp.get_info().await
            .map_err(|e| {
                tracing::error!("failed to get info for download: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::BAD_REQUEST)?;

        let filename = ytdlp::extract_filename(&info);
        resp = resp.header("Content-Disposition", format!("attachment; filename=\"{filename}\""));
    }

    ytdlp.start_download()
        .map_err(|e| {
            tracing::error!("failed to start download: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let stream = tokio_util::io::ReaderStream::new(ytdlp);
    let body = Body::from_stream(stream);

    resp.body(body)
        .map_err(|e| {
            tracing::error!("failed to stream response body: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
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
