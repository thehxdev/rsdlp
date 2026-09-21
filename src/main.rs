use axum::{Router, body::Body, extract::Form, response::Response, routing::{get, post}};
use serde::Deserialize;

mod ytdlp;

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
    match std::fs::read("index.html") {
        Ok(bytes) => Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => Response::builder()
            .status(500)
            .body(Body::from("index.html not found"))
            .unwrap(),
    }
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
