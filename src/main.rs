use axum::{Router, body::Body, extract::Form, response::Response, routing::post};
use serde::{Deserialize, Serialize};

mod ytdlp;

#[derive(Debug, Serialize, Deserialize)]
pub struct DownloadForm {
    pub url: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new().route("/download", post(download_handler));

    let bind_address = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    println!("listening on {bind_address}");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn download_handler(Form(form): Form<DownloadForm>) -> Response {
    let mut ytdlp = ytdlp::Ytdlp::new(&form.url);

    ytdlp
        .start(&["-S", "res:720"])
        .expect("Failed to start download");

    let stream = tokio_util::io::ReaderStream::new(ytdlp);
    let body = Body::from_stream(stream);

    Response::builder()
        .header("Content-Type", "application/octet-stream")
        .body(body)
        .unwrap()
}
