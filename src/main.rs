use std::sync::Arc;

use serde_json::Value;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

mod ytdlp;

const DEFAULT_MEDIA_TITLE: &'static str = "Unknown_Title";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        panic!("Usage: {} <MEDIA URL>", args[0]);
    }

    let mut ytdlp = ytdlp::Ytdlp::new(&args[1]);

    let media_info = ytdlp.get_media_info()
        .await?
        .expect("Media does not have any info");

    let title = if let Some(title) = media_info.get("title") {
        match title {
            Value::String(s) => s,
            _ => DEFAULT_MEDIA_TITLE,
        }
    } else {
        DEFAULT_MEDIA_TITLE
    };
    let title = String::from(title);

    let mut reader = ytdlp.start(&["-S", "res:480"])?;

    let ytdlp = Arc::new(Mutex::new(ytdlp));

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await
            .expect("Failed to get CTRL+C");

        _ = ytdlp.lock().await.terminate();
    });

    let dl_task = tokio::spawn(async move {
        let mut f = File::create(&format!("{title}.mp4"))
            .await
            .expect("Failed to open output file");

        let mut acc: usize = 0;
        let mut buffer = [0u8; 64 * 1024];
        while let Ok(nread) = reader.read(&mut buffer).await && nread > 0 {
            f.write_all(&buffer[..nread])
                .await
                .expect("Failed to write bytes to output file");

            acc += nread;
        }

        return acc;
    });

    let bytes_written = dl_task.await.unwrap();
    println!("Wrote {bytes_written} bytes to output file");

    Ok(())
}
