use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, Result};
use tokio::process::Command;
use tokio::sync::watch;

const BINARY: &str = "yt-dlp";

#[derive(Debug)]
pub struct Ytdlp {
    url: String,
    canceler: Option<watch::Sender<()>>,
}

impl Ytdlp {
    pub fn new(url: &str) -> Self {
        Self {
            url: String::from(url),
            canceler: None,
        }
    }

    pub async fn get_media_info(&self) -> Result<Option<Value>> {
        let output = Command::new(BINARY)
            .args(["-q", "-J", &self.url])
            .stderr(Stdio::inherit())
            .output()
            .await?;

        let output: Value = serde_json::from_slice(&output.stdout)?;

        if let Value::Object(_) = output {
            Ok(Some(output))
        } else {
            Ok(None)
        }
    }

    pub fn start(&mut self, extra_args: &[&str]) -> Result<impl AsyncReadExt + 'static> {
        let mut ytdlp_args = vec!["-o", "-"];
        ytdlp_args.extend_from_slice(extra_args);
        ytdlp_args.push(&self.url);

        let mut child = Command::new(BINARY)
            .args(&ytdlp_args)
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .expect("Failed to get yt-dlp child process stdout");

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

        Ok(stdout)
    }

    pub fn cancel_handle(&self) -> Option<watch::Sender<()>> {
        self.canceler.clone()
    }

    pub fn terminate(&mut self) {
        if let Some(canceler) = &self.canceler {
            _ = canceler.send(());
        }
    }
}

impl Drop for Ytdlp {
    fn drop(&mut self) {
        self.terminate();
    }
}
