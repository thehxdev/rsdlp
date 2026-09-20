use std::process::Stdio;
use tokio::io::{AsyncReadExt, Result};
use tokio::process::Command;
use tokio::sync::oneshot::{channel, Sender};
use serde_json::Value;

#[derive(Debug)]
pub struct Ytdlp {
    binary_path: String,
    url: String,
}

impl Ytdlp {
    pub fn new(url: &str) -> Self {
        // TODO: Curretly the yt-dlp binary path is hardcoded. Make it configurable.
        Self {
            binary_path: String::from("yt-dlp"),
            url: String::from(url),
        }
    }

    pub async fn get_media_info(&self) -> Result<Option<Value>> {
        use serde_json::Value;
        let output = Command::new(&self.binary_path)
            .args(&["-q", "-J", &self.url])
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

    pub fn start(&mut self, extra_args: &[&str]) -> Result<(impl AsyncReadExt + 'static, Sender<()>)> {
        let mut ytdlp_args = vec!["-o", "-"];
        ytdlp_args.extend_from_slice(extra_args);
        ytdlp_args.push(&self.url);

        let mut child = Command::new(&self.binary_path)
            .args(&ytdlp_args)
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;

        let stdout = child.stdout.take()
            .expect("Failed to get yt-dlp child process stdout");

        let (send, recv) = channel::<()>();
        let child_pid = child.id().unwrap() as i32;

        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = recv => {
                    // NOTE: Currently I couldn't find a cross-platform way to kill a parent process
                    // and all of it's children. Crates like `process-wrap` are also broken and vibe
                    // coded slop. So I assume my target platform is Unix-like systems that support
                    // `getpgid`, `kill` and `waitpid`.
                    // To kill a process with all of it's children we have to get the process group
                    // id (pgid) and send SIGTERM to the group id. All child processes have same
                    // group ids.
                    unsafe {
                        let pgid = libc::getpgid(child_pid);
                        if pgid == -1 { return; }
                        _ = libc::kill(pgid, libc::SIGTERM);
                        _ = libc::waitpid(child_pid, std::ptr::null_mut(), 0);
                    }
                    println!("yt-dlp process terminated with SIGTERM");
                }
            }
        });

        Ok((stdout, send))
    }
}
