use axum::{Router, body::Body, extract::Form, response::Response, routing::{get, post}};
use serde::Deserialize;

mod ytdlp;

const INDEX_HTML: &[u8] = include_bytes!("../index.html");

#[derive(Deserialize)]
struct DownloadForm {
    url: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/download", post(download_handler));

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

async fn download_handler(Form(form): Form<DownloadForm>) -> Response {
    let mut ytdlp = ytdlp::Ytdlp::new(&form.url);

    ytdlp.start_download().expect("Failed to start download");

    let stream = tokio_util::io::ReaderStream::new(ytdlp);
    let body = Body::from_stream(stream);

    Response::builder()
        .header("Content-Type", "application/octet-stream")
        .body(body)
        .unwrap()
}
