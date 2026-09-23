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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    load_dotenv();

    let args: Vec<String> = std::env::args().collect();

    if let Some(tg_config) = telegram::TelegramConfig::from_env() {
        if args.iter().any(|arg| arg == "--tg-login") {
            println!("Running Telegram interactive login...");
            let session = std::sync::Arc::new(grammers_session::storages::SqliteSession::open(&tg_config.session_file)?);
            let pool = grammers_mtsender::SenderPool::new(std::sync::Arc::clone(&session), tg_config.api_id);
            let client = grammers_client::Client::new(&pool);
            tokio::spawn(pool.runner.run());
            telegram::authorize_client(&client, &tg_config.api_hash, tg_config.bot_token.as_deref()).await?;
            println!("Login complete. Session saved to {}", tg_config.session_file);
            return Ok(());
        }

        tokio::spawn(async move {
            if let Err(e) = telegram::run_bot(tg_config).await {
                eprintln!("[telegram] bot error: {e}");
            }
        });
    } else {
        println!("Telegram credentials not set (RSDLP_TG_API_ID / RSDLP_TG_API_HASH). Running web-only mode.");
    }

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/download", post(download_handler))
        .route("/qualities", post(qualities_handler));

    let bind_address = std::env::var("RSDLP_BIND_ADDRESS")
        .or_else(|_| std::env::var("BIND_ADDRESS"))
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_address).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
