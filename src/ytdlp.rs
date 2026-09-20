use serde_json::Value;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, Result};
use tokio::process::{ChildStdout, Command};
use tokio::sync::watch;

const BINARY: &str = "yt-dlp";

#[derive(Debug)]
pub struct Ytdlp {
    url: String,
    child_stdout: Option<ChildStdout>,
    canceler: Option<watch::Sender<()>>,
}

impl Ytdlp {
    pub fn new(url: &str) -> Self {
        Self {
            url: String::from(url),
            child_stdout: None,
            canceler: None,
        }
    }

    pub async fn get_info(&self) -> Result<Option<Value>> {
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

    pub fn start_download(&mut self, extra_args: &[&str]) -> Result<()> {
        let mut ytdlp_args = vec!["-o", "-"];
        ytdlp_args.extend_from_slice(extra_args);
        ytdlp_args.push(&self.url);

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
        // println!("ytdlp instance dropped and it's process group terminated");
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
