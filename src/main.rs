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
                eprintln!("invalid {name}: {e}");
                StatusCode::BAD_REQUEST
            }),
    }
}

#[derive(Deserialize)]
struct QualitiesForm {
    url: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();

    if let Some(tg_config) = telegram::TelegramConfig::from_env() {
        if args.iter().any(|arg| arg == "--tg-login") {
            println!("Running Telegram interactive login...");
            let session = std::sync::Arc::new(grammers_session::storages::SqliteSession::open(&tg_config.session_file)?);
            let pool = grammers_mtsender::SenderPool::new(std::sync::Arc::clone(&session), tg_config.api_id);
            let client = grammers_client::Client::new(&pool);
            tokio::spawn(pool.runner.run());
            telegram::authorize_client(&client, &tg_config.api_hash).await?;
            println!("Login complete. Session saved to {}", tg_config.session_file);
            return Ok(());
        }

        tokio::spawn(async move {
            if let Err(e) = telegram::run_bot(tg_config).await {
                eprintln!("[telegram] bot error: {e}");
            }
        });
    } else {
        println!("Telegram credentials not set (TG_API_ID / TG_API_HASH). Running web-only mode.");
    }

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/download", post(download_handler))
        .route("/qualities", post(qualities_handler));

    let bind_address = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    println!("listening on {bind_address}");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn index_handler() -> Response {
    Response::builder()
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Body::from(INDEX_HTML))
        .unwrap()
}

async fn qualities_handler(
    Form(form): Form<QualitiesForm>,
) -> Result<Json<ytdlp::Qualities>, StatusCode> {
    let ytdlp = ytdlp::Ytdlp::new(&form.url, None);

    let info = ytdlp.get_info().await
        .map_err(|e| {
            eprintln!("{e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::BAD_REQUEST)?;

    Ok(Json(ytdlp::extract_qualities(&info)))
}

async fn download_handler(Form(form): Form<DownloadForm>) -> Result<Response, StatusCode> {
    let res = parse_quality("res", form.res)?;
    let abr = parse_quality("abr", form.abr)?;

    if res.is_none() && abr.is_none() {
        eprintln!("missing quality: at least one of res or abr must be specified");
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut resp = Response::builder();
    let mut ytdlp = ytdlp::Ytdlp::new(&form.url, Some(ytdlp::build_sort(res, abr)));

    {
        let info = ytdlp.get_info().await
            .map_err(|e| {
                eprintln!("{e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::BAD_REQUEST)?;

        let filename = ytdlp::extract_filename(&info);
        resp = resp.header("Content-Disposition", format!("attachment; filename=\"{filename}\""));
    }

    ytdlp.start_download()
        .map_err(|e| {
            eprintln!("{e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let stream = tokio_util::io::ReaderStream::new(ytdlp);
    let body = Body::from_stream(stream);

    resp.body(body)
        .map_err(|e| {
            eprintln!("{e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}
