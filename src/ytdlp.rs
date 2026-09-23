use std::collections::BTreeSet;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, Result};
use tokio::process::{ChildStdout, Command};
use tokio::sync::watch;
use serde::Serialize;
use serde_json::Value;

const BINARY: &str = "yt-dlp";
// NOTE: `-S` is deliberately not part of these. It is appended per request so
// the caller can choose the quality: `-S res:<px>,abr:<kbps>`.
const COMMON_FLAGS: &[&str] = &[
    // "-v",
    "--abort-on-error",
    "--no-playlist",
    "--retries", "5",
    "--downloader-args", "ffmpeg:-preset ultrafast -c copy",
    "--no-embed-thumbnail",
    "--no-embed-metadata",
    "-f", "b/bv*+ba",
];

/// Well-known quality steps, so the UI only ever offers standard
/// alternatives (360p/480p/720p/… and 128/192/320 kbps).
const VIDEO_STEPS: &[u64] = &[144, 240, 360, 480, 720, 1080, 1440, 2160, 4320];
const AUDIO_STEPS: &[u64] = &[128, 192, 320];

/// The resolutions a media offers, in the units of the matching `-S` field:
/// `video` is the smallest dimension in pixels (`-S res`), `audio` is the
/// average bitrate in kbps (`-S abr`). Each value is rounded **up** to the
/// next standard step — `-S` is a cap that prefers the best format at or
/// below it, so every offered step still selects an existing format.
/// Sorted ascending, deduplicated.
#[derive(Debug, Serialize)]
pub struct Qualities {
    pub video: Vec<u64>,
    pub audio: Vec<u64>,
}

#[derive(Debug)]
pub struct Ytdlp {
    url: String,
    sort: Option<String>,
    child_stdout: Option<ChildStdout>,
    canceler: Option<watch::Sender<()>>,
}

impl Ytdlp {
    pub fn new(url: &str, sort: Option<String>) -> Self {
        Self {
            url: String::from(url),
            sort,
            child_stdout: None,
            canceler: None,
        }
    }

    pub async fn get_info(&self) -> Result<Option<Value>> {
        let mut ytdlp_args = vec![
            "-J",
            &self.url
        ];
        ytdlp_args.extend_from_slice(COMMON_FLAGS);
        if let Some(sort) = &self.sort {
            ytdlp_args.extend(["-S", sort.as_str()]);
        }

        let output = Command::new(BINARY)
            .args(&ytdlp_args)
            .output()
            .await?;

        let info: Value = serde_json::from_slice(&output.stdout)?;

        Ok(match info {
            Value::Object(_) => Some(info),
            _ => None,
        })
    }

    pub fn start_download(&mut self) -> Result<()> {
        let mut ytdlp_args = vec![
            "-o", "-",
            &self.url
        ];
        ytdlp_args.extend_from_slice(COMMON_FLAGS);
        if let Some(sort) = &self.sort {
            ytdlp_args.extend(["-S", sort.as_str()]);
        }

        let mut child = Command::new(BINARY)
            .args(&ytdlp_args)
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .expect("Failed to get yt-dlp child process stdout");
        self.child_stdout = Some(stdout);

        let (send, mut recv) = watch::channel(());
        let child_pid = child.id().unwrap() as i32;
        self.canceler = Some(send);

        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = recv.changed() => {
                    // NOTE: Currently I couldn't find a cross-platform way to kill a parent process
                    // and all of it's children. Crates like `process-wrap` are also broken and vibe
                    // coded slop. So I assume my target platform is Unix-like systems that support
                    // `getpgid` and `kill`.
                    // To kill a process with all of it's children we have to get the process group
                    // id (pgid) and send SIGTERM to the group id. All child processes have same
                    // group ids.
                    unsafe {
                        let pgid = libc::getpgid(child_pid);
                        if pgid == -1 { return; }
                        _ = libc::kill(pgid, libc::SIGTERM);
                    }
                    _ = child.wait().await;
                }
            }
        });

        Ok(())
    }
}

/// Round values up onto the standard steps and re-deduplicate.
fn snap(values: &BTreeSet<u64>, steps: &[u64]) -> Vec<u64> {
    values
        .iter()
        .map(|&value| {
            steps
                .iter()
                .copied()
                .find(|&step| step >= value)
                .unwrap_or(value)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Extract the available resolutions from a `yt-dlp -J` info dict.
///
/// Video formats are described by their dimensions in pixels, audio-only
/// formats by their bitrate in kbps — hence the two lists with two units.
pub fn extract_qualities(info: &Value) -> Qualities {
    let mut video = BTreeSet::new();
    let mut audio = BTreeSet::new();

    let formats = info
        .get("formats")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    for format in formats {
        let vcodec = format.get("vcodec").and_then(Value::as_str);
        let acodec = format.get("acodec").and_then(Value::as_str);
        let width = format.get("width").and_then(Value::as_u64);
        let height = format.get("height").and_then(Value::as_u64);

        // `-S res` sorts by the smallest dimension, so offer exactly that.
        if vcodec != Some("none")
            && let Some(height) = height
        {
            video.insert(width.map_or(height, |width| width.min(height)));
        }

        if acodec != Some("none") {
            // `abr` can be fractional (e.g. bilibili's 68.646). Ceil it so the
            // offered value still covers the format when used as `-S abr:N`.
            if let Some(abr) = format.get("abr").and_then(Value::as_f64) {
                audio.insert(abr.ceil() as u64);
            }
        }
    }

    Qualities {
        video: snap(&video, VIDEO_STEPS),
        audio: snap(&audio, AUDIO_STEPS),
    }
}

impl Drop for Ytdlp {
    fn drop(&mut self) {
        if let Some(canceler) = &self.canceler {
            _ = canceler.send(());
        }
    }
}

impl AsyncRead for Ytdlp {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some(child_stdout) = &mut self.child_stdout {
            Pin::new(child_stdout).poll_read(cx, buf)
        } else {
            Poll::Pending
        }
    }
}

/// Build the `-S` sort expression from requested quality parameters.
pub fn build_sort(res: Option<u64>, abr: Option<u64>) -> String {
    let mut parts = Vec::new();
    if let Some(res) = res {
        parts.push(format!("res:{res}"));
    }
    if let Some(abr) = abr {
        parts.push(format!("abr:{abr}"));
    }
    parts.join(",")
}

/// Extract downloaded filename or fallback from yt-dlp info json.
pub fn extract_filename(info: &Value) -> String {
    let requested_download = info
        .get("requested_downloads")
        .and_then(Value::as_array)
        .and_then(|arr| arr.get(0))
        .and_then(Value::as_object);

    if let Some(dl) = requested_download {
        if let Some(Value::String(s)) = dl.get("filename") {
            return s.clone();
        }
        let ext = dl.get("ext").and_then(Value::as_str).unwrap_or("bin");
        return format!("unknown_title.{ext}");
    }

    let ext = info.get("ext").and_then(Value::as_str).unwrap_or("bin");
    format!("unknown_title.{ext}")
}

/// Extract media title or fallback from yt-dlp info json.
pub fn extract_title(info: &Value) -> String {
    info.get("title")
        .and_then(Value::as_str)
        .unwrap_or("Media")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_build_sort() {
        assert_eq!(build_sort(Some(720), None), "res:720");
        assert_eq!(build_sort(None, Some(192)), "abr:192");
        assert_eq!(build_sort(Some(1080), Some(320)), "res:1080,abr:320");
        assert_eq!(build_sort(None, None), "");
    }

    #[test]
    fn test_extract_filename() {
        let info = json!({
            "requested_downloads": [
                { "filename": "sample_video.mp4", "ext": "mp4" }
            ]
        });
        assert_eq!(extract_filename(&info), "sample_video.mp4");

        let fallback_info = json!({
            "requested_downloads": [
                { "ext": "mkv" }
            ]
        });
        assert_eq!(extract_filename(&fallback_info), "unknown_title.mkv");
    }

    #[test]
    fn test_extract_title() {
        let info = json!({ "title": "My Test Video" });
        assert_eq!(extract_title(&info), "My Test Video");

        let empty = json!({});
        assert_eq!(extract_title(&empty), "Media");
    }
}
