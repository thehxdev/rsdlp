use serde_json::Value;
use tokio::fs::File;

mod ytdlp;

const DEFAULT_MEDIA_TITLE: &str = "Unknown_Title";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        panic!("Usage: {} <MEDIA URL>", args[0]);
    }

    let mut ytdlp = ytdlp::Ytdlp::new(&args[1]);

    let media_info = ytdlp
        .get_media_info()
        .await?
        .expect("Media does not have any info");

    let filename = format!(
        "{}.mp4",
        media_info
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_MEDIA_TITLE),
    );

    let mut reader = ytdlp.start(&["-S", "res:480"])?;
    let canceler = ytdlp.cancel_handle().expect("yt-dlp did not start");

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Failed to get CTRL+C");

        _ = canceler.send(());
    });

    let mut f = File::create(&filename)
        .await
        .expect("Failed to open output file");

    let bytes_written = tokio::io::copy(&mut reader, &mut f)
        .await
        .expect("Failed to write bytes to output file");

    println!("Wrote {bytes_written} bytes to output file");

    Ok(())
}
