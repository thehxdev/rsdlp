use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead as _, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use grammers_client::client::{Client, SignInError, UpdatesConfiguration};
use grammers_client::types::update;
use grammers_client::types::{InputMessage, Update};
use grammers_client::{button, reply_markup};
use grammers_mtsender::SenderPool;
use grammers_session::defs::{PeerId, PeerRef};
use grammers_session::storages::SqliteSession;

use crate::ytdlp::{self, Qualities, Ytdlp};

static DOWNLOAD_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, PartialEq, Clone)]
pub enum TelegramAction {
    Help,
    Cancel,
    DownloadUrl { url: String },
    SelectQuality { res: Option<u64>, abr: Option<u64> },
    Unknown,
}

/// Extract first http/https URL token from text if present.
fn find_url(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|word| {
        if word.starts_with("http://") || word.starts_with("https://") {
            Some(word.to_string())
        } else {
            None
        }
    })
}

pub fn parse_message_action(text: &str) -> TelegramAction {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return TelegramAction::Unknown;
    }

    if trimmed == "/start" || trimmed == "/help" {
        return TelegramAction::Help;
    }

    if trimmed == "/cancel" {
        return TelegramAction::Cancel;
    }

    if trimmed == "/dl_best" {
        return TelegramAction::SelectQuality {
            res: None,
            abr: None,
        };
    }

    if let Some(stripped) = trimmed.strip_prefix("/dl_audio_") {
        if let Ok(abr) = stripped.parse::<u64>() {
            return TelegramAction::SelectQuality {
                res: None,
                abr: Some(abr),
            };
        }
    }

    if let Some(stripped) = trimmed.strip_prefix("/dl_") {
        if let Ok(res) = stripped.parse::<u64>() {
            return TelegramAction::SelectQuality {
                res: Some(res),
                abr: None,
            };
        }
    }

    if let Some(stripped) = trimmed.strip_prefix("/dl ") {
        let parts: Vec<&str> = stripped.split_whitespace().collect();
        if parts.len() == 1 {
            if let Ok(num) = parts[0].parse::<u64>() {
                return TelegramAction::SelectQuality {
                    res: Some(num),
                    abr: None,
                };
            }
        } else if parts.len() == 2 && parts[0] == "audio" {
            if let Ok(num) = parts[1].parse::<u64>() {
                return TelegramAction::SelectQuality {
                    res: None,
                    abr: Some(num),
                };
            }
        }
    }

    if let Some(url) = find_url(trimmed) {
        return TelegramAction::DownloadUrl { url };
    }

    TelegramAction::Unknown
}

/// Formats qualities into a Telegram message with clickable commands.
pub fn format_qualities_menu(title: &str, qualities: &Qualities) -> String {
    let mut msg = format!("🎬 {title}\n\nChoose quality to download:\n\n");

    if !qualities.video.is_empty() {
        msg.push_str("📹 Video:\n");
        for &res in qualities.video.iter().rev() {
            msg.push_str(&format!("  /dl_{res} — {res}p\n"));
        }
        msg.push('\n');
    }

    if !qualities.audio.is_empty() {
        msg.push_str("🎵 Audio only:\n");
        for &abr in qualities.audio.iter().rev() {
            msg.push_str(&format!("  /dl_audio_{abr} — {abr} kbps\n"));
        }
        msg.push('\n');
    }

    msg.push_str("⚡ /dl_best — Best available\n");
    msg.push_str("❌ /cancel — Cancel");
    msg
}

/// Builds inline keyboard markup for video, audio, best, and cancel options.
pub fn build_qualities_inline_markup(qualities: &Qualities) -> reply_markup::Inline {
    let mut rows: Vec<Vec<button::Inline>> = Vec::new();

    if !qualities.video.is_empty() {
        let mut video_row = Vec::new();
        for &res in qualities.video.iter().rev() {
            video_row.push(button::inline(format!("{res}p"), format!("dl:{res}")));
            if video_row.len() == 3 {
                rows.push(std::mem::take(&mut video_row));
            }
        }
        if !video_row.is_empty() {
            rows.push(video_row);
        }
    }

    if !qualities.audio.is_empty() {
        let mut audio_row = Vec::new();
        for &abr in qualities.audio.iter().rev() {
            audio_row.push(button::inline(format!("🎵 {abr}k"), format!("dl_audio:{abr}")));
            if audio_row.len() == 3 {
                rows.push(std::mem::take(&mut audio_row));
            }
        }
        if !audio_row.is_empty() {
            rows.push(audio_row);
        }
    }

    rows.push(vec![
        button::inline("⚡ Best", "dl:best"),
        button::inline("❌ Cancel", "cancel"),
    ]);

    reply_markup::inline(rows)
}

/// Builds inline markup containing a Cancel button.
pub fn cancel_markup() -> reply_markup::Inline {
    reply_markup::inline(vec![vec![button::inline("❌ Cancel", "cancel")]])
}

/// Parses inline button callback data into corresponding TelegramAction.
pub fn parse_callback_action(data: &[u8]) -> TelegramAction {
    let s = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return TelegramAction::Unknown,
    };
    if s == "cancel" {
        return TelegramAction::Cancel;
    }
    if s == "dl:best" {
        return TelegramAction::SelectQuality {
            res: None,
            abr: None,
        };
    }
    if let Some(res_str) = s.strip_prefix("dl:") {
        if let Ok(res) = res_str.parse::<u64>() {
            return TelegramAction::SelectQuality {
                res: Some(res),
                abr: None,
            };
        }
    }
    if let Some(abr_str) = s.strip_prefix("dl_audio:") {
        if let Ok(abr) = abr_str.parse::<u64>() {
            return TelegramAction::SelectQuality {
                res: None,
                abr: Some(abr),
            };
        }
    }
    TelegramAction::Unknown
}

/// RAII guard ensuring temporary downloaded files are unconditionally deleted upon drop.
pub struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Debug)]
pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: Option<String>,
    pub session_file: String,
}

impl TelegramConfig {
    pub fn from_env() -> Option<Self> {
        let get_var = |key: &str| {
            env::var(format!("RSDLP_{key}"))
                .ok()
                .or_else(|| env::var(key).ok())
        };

        let api_id = get_var("TG_API_ID")?.parse().ok()?;
        let api_hash = get_var("TG_API_HASH")?;
        let bot_token = get_var("TG_BOT_TOKEN").filter(|s| !s.trim().is_empty());
        let session_file =
            get_var("TG_SESSION_FILE").unwrap_or_else(|| "rsdlp.session".to_string());

        Some(Self {
            api_id,
            api_hash,
            bot_token,
            session_file,
        })
    }
}

fn prompt(message: &str) -> io::Result<String> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(message.as_bytes())?;
    stdout.flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

pub async fn authorize_client(
    client: &Client,
    api_hash: &str,
    bot_token: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if client.is_authorized().await? {
        return Ok(());
    }

    if let Some(token) = bot_token {
        tracing::info!("Telegram session not authorized. Signing in with bot token...");
        client.bot_sign_in(token, api_hash).await?;
        tracing::info!("Telegram bot token login successful");
        return Ok(());
    }

    println!("[telegram] Session not authorized. Starting interactive login...");
    let phone = prompt("Enter phone number with country code (e.g. +1234567890): ")?;
    let token = client.request_login_code(&phone, api_hash).await?;
    let code = prompt("Enter the login code you received: ")?;

    match client.sign_in(&token, &code).await {
        Ok(_) => {
            println!("[telegram] Login successful!");
            Ok(())
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            let hint = password_token.hint().unwrap_or("none");
            let prompt_msg = format!("2FA Password required (hint: {hint}): ");
            let password = prompt(&prompt_msg)?;
            client.check_password(password_token, &password).await?;
            println!("[telegram] 2FA authentication successful!");
            Ok(())
        }
        Err(e) => Err(Box::new(e)),
    }
}

pub struct PendingChoice {
    pub url: String,
    pub title: String,
}

pub struct BotState {
    pub pending: Mutex<HashMap<PeerId, PendingChoice>>,
    pub active_operations: Mutex<HashMap<PeerId, tokio::task::AbortHandle>>,
}

impl BotState {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            active_operations: Mutex::new(HashMap::new()),
        }
    }

    pub async fn cancel(&self, peer_id: &PeerId) -> bool {
        let mut cancelled = false;
        if self.pending.lock().await.remove(peer_id).is_some() {
            cancelled = true;
        }
        if let Some(handle) = self.active_operations.lock().await.remove(peer_id) {
            handle.abort();
            cancelled = true;
        }
        cancelled
    }
}

pub async fn run_bot(
    config: TelegramConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session = Arc::new(SqliteSession::open(&config.session_file)?);
    let pool = SenderPool::new(Arc::clone(&session), config.api_id);
    let client = Client::new(&pool);
    let _pool_task = tokio::spawn(pool.runner.run());

    authorize_client(&client, &config.api_hash, config.bot_token.as_deref()).await?;

    let me = client.get_me().await?;
    let me_peer_id = PeerId::user(me.raw.id());
    tracing::info!(
        "[telegram] userbot running as {} (id: {})",
        me.first_name().unwrap_or("User"),
        me.raw.id()
    );

    let state = Arc::new(BotState::new());
    let mut update_stream = client.stream_updates(
        pool.updates,
        UpdatesConfiguration {
            catch_up: true,
            ..Default::default()
        },
    );

    while let Ok(update) = update_stream.next().await {
        match update {
            Update::NewMessage(message) => {
                let peer_id = message.peer_id();
                let is_saved_messages = peer_id == me_peer_id;
                // Process if incoming message, or user typing to self in Saved Messages
                if !message.outgoing() || is_saved_messages {
                    let client_clone = client.clone();
                    let state_clone = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_message(client_clone, state_clone, message).await {
                            tracing::error!("[telegram] error handling message: {e}");
                        }
                    });
                }
            }
            Update::CallbackQuery(query) => {
                let client_clone = client.clone();
                let state_clone = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = handle_callback_query(client_clone, state_clone, query).await {
                        tracing::error!("[telegram] error handling callback query: {e}");
                    }
                });
            }
            _ => {}
        }
    }

    Ok(())
}

fn peer_ref_from_message(message: &update::Message) -> PeerRef {
    match message.peer() {
        Ok(p) => p.into(),
        Err(r) => r,
    }
}

async fn handle_callback_query(
    client: Client,
    state: Arc<BotState>,
    query: update::CallbackQuery,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let peer_id = query.peer().id();
    let action = parse_callback_action(query.data());

    match action {
        TelegramAction::Cancel => {
            let was_cancelled = state.cancel(&peer_id).await;
            let text = if was_cancelled {
                "❌ Operation cancelled."
            } else {
                "Nothing to cancel."
            };
            let _ = query.answer().edit(InputMessage::new().text(text)).await;
        }
        TelegramAction::SelectQuality { res, abr } => {
            let pending_opt = state.pending.lock().await.remove(&peer_id);
            let pending = match pending_opt {
                Some(p) => p,
                None => {
                    let _ = query
                        .answer()
                        .alert("No pending media URL. Send a URL first!")
                        .send()
                        .await;
                    return Ok(());
                }
            };

            let _ = query.answer().send().await;

            let desc = match (res, abr) {
                (Some(r), _) => format!("{r}p"),
                (None, Some(a)) => format!("{a} kbps audio"),
                (None, None) => "best quality".to_string(),
            };

            let peer = query.peer().clone();
            let msg_id = query.load_message().await.map(|m| m.id()).ok();

            let state_clone = Arc::clone(&state);
            let client_clone = client.clone();
            let peer_id_copy = peer_id;

            let join_handle = tokio::spawn(async move {
                let res = execute_download_and_upload(
                    &client_clone,
                    &peer,
                    pending,
                    res,
                    abr,
                    &desc,
                    msg_id,
                )
                .await;
                state_clone
                    .active_operations
                    .lock()
                    .await
                    .remove(&peer_id_copy);
                if let Err(e) = res {
                    tracing::error!("[telegram] download/upload error: {e}");
                }
            });

            state
                .active_operations
                .lock()
                .await
                .insert(peer_id, join_handle.abort_handle());
        }
        _ => {
            let _ = query.answer().send().await;
        }
    }

    Ok(())
}

async fn handle_message(
    client: Client,
    state: Arc<BotState>,
    message: update::Message,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let peer_id = message.peer_id();
    let text = message.text();
    let action = parse_message_action(text);

    match action {
        TelegramAction::Help => {
            let help_text = "rsdlp Telegram Bot\n\nSend me any media URL (YouTube, Twitter/X, TikTok, etc.) to fetch available qualities and download.";
            message.respond(InputMessage::new().text(help_text)).await?;
        }
        TelegramAction::Cancel => {
            let was_cancelled = state.cancel(&peer_id).await;
            let text = if was_cancelled {
                "❌ Operation cancelled."
            } else {
                "Nothing to cancel."
            };
            message.respond(InputMessage::new().text(text)).await?;
        }
        TelegramAction::DownloadUrl { url } => {
            let status_msg = message
                .respond(InputMessage::new().text("🔍 Fetching qualities..."))
                .await?;
            let ytdlp = Ytdlp::new(&url, None);
            match ytdlp.get_info().await {
                Ok(Some(info)) => {
                    let title = ytdlp::extract_title(&info);
                    let qualities = ytdlp::extract_qualities(&info);

                    if qualities.video.is_empty() && qualities.audio.is_empty() {
                        status_msg
                            .edit(InputMessage::new().text(
                                "No playable formats found for this URL.",
                            ))
                            .await?;
                        return Ok(());
                    }

                    state.pending.lock().await.insert(
                        peer_id,
                        PendingChoice {
                            url: url.clone(),
                            title: title.clone(),
                        },
                    );

                    let menu = format_qualities_menu(&title, &qualities);
                    let markup = build_qualities_inline_markup(&qualities);
                    status_msg
                        .edit(InputMessage::new().text(menu).reply_markup(&markup))
                        .await?;
                }
                _ => {
                    status_msg
                        .edit(InputMessage::new().text("Failed to fetch media metadata."))
                        .await?;
                }
            }
        }
        TelegramAction::SelectQuality { res, abr } => {
            let pending_opt = state.pending.lock().await.remove(&peer_id);
            let pending = match pending_opt {
                Some(p) => p,
                None => {
                    message
                        .respond(InputMessage::new().text(
                            "No pending media URL. Send a URL first!",
                        ))
                        .await?;
                    return Ok(());
                }
            };

            let desc = match (res, abr) {
                (Some(r), _) => format!("{r}p"),
                (None, Some(a)) => format!("{a} kbps audio"),
                (None, None) => "best quality".to_string(),
            };

            let peer_ref = peer_ref_from_message(&message);
            let state_clone = Arc::clone(&state);
            let client_clone = client.clone();
            let peer_id_copy = peer_id;

            let join_handle = tokio::spawn(async move {
                let res = execute_download_and_upload(
                    &client_clone,
                    peer_ref,
                    pending,
                    res,
                    abr,
                    &desc,
                    None,
                )
                .await;
                state_clone
                    .active_operations
                    .lock()
                    .await
                    .remove(&peer_id_copy);
                if let Err(e) = res {
                    tracing::error!("[telegram] download/upload error: {e}");
                }
            });

            state
                .active_operations
                .lock()
                .await
                .insert(peer_id, join_handle.abort_handle());
        }
        TelegramAction::Unknown => {}
    }

    Ok(())
}

async fn execute_download_and_upload<P: Into<PeerRef>>(
    client: &Client,
    peer: P,
    pending: PendingChoice,
    res: Option<u64>,
    abr: Option<u64>,
    desc: &str,
    status_msg_id: Option<i32>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let peer_ref = peer.into();
    let sort_str = ytdlp::build_sort(res, abr);
    let mut ytdlp = Ytdlp::new(
        &pending.url,
        if sort_str.is_empty() {
            None
        } else {
            Some(sort_str)
        },
    );

    let downloading_msg = InputMessage::new()
        .text(format!("⏳ Downloading {} ({desc})...", pending.title))
        .reply_markup(&cancel_markup());

    let status_msg_id = match status_msg_id {
        Some(id) => {
            let _ = client.edit_message(peer_ref, id, downloading_msg).await;
            id
        }
        None => {
            let msg = client.send_message(peer_ref, downloading_msg).await?;
            msg.id()
        }
    };

    let filename = match ytdlp.get_info().await {
        Ok(Some(info)) => ytdlp::extract_filename(&info),
        _ => "video.mp4".to_string(),
    };

    let id = DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let safe_filename = filename.replace(['/', '\\', '\0'], "_");
    let temp_path = PathBuf::from(std::env::temp_dir()).join(format!(
        "rsdlp_{}_{}_{}",
        std::process::id(),
        id,
        safe_filename
    ));
    let guard = TempFileGuard::new(temp_path);

    let download_res = async {
        ytdlp.start_download()?;
        let mut temp_file = tokio::fs::File::create(guard.path()).await?;
        tokio::io::copy(&mut ytdlp, &mut temp_file).await?;
        temp_file.flush().await?;
        Ok::<(), std::io::Error>(())
    }
    .await;

    if let Err(e) = download_res {
        let _ = client
            .edit_message(
                peer_ref,
                status_msg_id,
                InputMessage::new().text(format!("Download failed: {e}")),
            )
            .await;
        return Ok(());
    }

    let uploading_msg = InputMessage::new()
        .text(format!("📤 Uploading {} to Telegram...", pending.title))
        .reply_markup(&cancel_markup());
    let _ = client
        .edit_message(peer_ref, status_msg_id, uploading_msg)
        .await;

    let upload_res = client.upload_file(guard.path()).await;

    match upload_res {
        Ok(uploaded) => {
            let caption = format!("🎬 {}\nQuality: {desc}", pending.title);
            let input_file = InputMessage::new().file(uploaded).text(caption);
            client.send_message(peer_ref, input_file).await?;
            let _ = client.delete_messages(peer_ref, &[status_msg_id]).await;
        }
        Err(e) => {
            let _ = client
                .edit_message(
                    peer_ref,
                    status_msg_id,
                    InputMessage::new().text(format!("Upload failed: {e}")),
                )
                .await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_message_action() {
        assert_eq!(parse_message_action("/start"), TelegramAction::Help);
        assert_eq!(parse_message_action("/cancel"), TelegramAction::Cancel);
        assert_eq!(
            parse_message_action("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            TelegramAction::DownloadUrl {
                url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string()
            }
        );
        assert_eq!(
            parse_message_action("Check this: https://example.com/video.mp4 cool"),
            TelegramAction::DownloadUrl {
                url: "https://example.com/video.mp4".to_string()
            }
        );
        assert_eq!(
            parse_message_action("/dl_720"),
            TelegramAction::SelectQuality {
                res: Some(720),
                abr: None,
            }
        );
        assert_eq!(
            parse_message_action("/dl 1080"),
            TelegramAction::SelectQuality {
                res: Some(1080),
                abr: None,
            }
        );
        assert_eq!(
            parse_message_action("/dl_audio_320"),
            TelegramAction::SelectQuality {
                res: None,
                abr: Some(320)
            }
        );
        assert_eq!(
            parse_message_action("/dl_best"),
            TelegramAction::SelectQuality {
                res: None,
                abr: None,
            }
        );
    }

    #[test]
    fn test_format_qualities_menu() {
        let qualities = Qualities {
            video: vec![360, 720, 1080],
            audio: vec![128, 320],
        };
        let menu = format_qualities_menu("Test Title", &qualities);
        assert!(menu.contains("🎬 Test Title"));
        assert!(menu.contains("/dl_1080 — 1080p"));
        assert!(menu.contains("/dl_720 — 720p"));
        assert!(menu.contains("/dl_audio_320 — 320 kbps"));
        assert!(menu.contains("/dl_best"));
        assert!(menu.contains("/cancel"));
    }

    #[test]
    fn test_parse_callback_action() {
        assert_eq!(parse_callback_action(b"cancel"), TelegramAction::Cancel);
        assert_eq!(
            parse_callback_action(b"dl:best"),
            TelegramAction::SelectQuality {
                res: None,
                abr: None
            }
        );
        assert_eq!(
            parse_callback_action(b"dl:1080"),
            TelegramAction::SelectQuality {
                res: Some(1080),
                abr: None
            }
        );
        assert_eq!(
            parse_callback_action(b"dl_audio:320"),
            TelegramAction::SelectQuality {
                res: None,
                abr: Some(320)
            }
        );
        assert_eq!(parse_callback_action(b"unknown_cmd"), TelegramAction::Unknown);
    }

    #[test]
    fn test_build_qualities_inline_markup() {
        let qualities = Qualities {
            video: vec![360, 720, 1080],
            audio: vec![128, 320],
        };
        let _markup = build_qualities_inline_markup(&qualities);
    }

    #[test]
    fn test_cancel_markup() {
        let _markup = cancel_markup();
    }

    #[tokio::test]
    async fn test_bot_state_cancel() {
        let state = BotState::new();
        let peer_id = PeerId::user(42);

        // Test cancel on empty state
        assert!(!state.cancel(&peer_id).await);

        // Test cancel with pending choice
        state.pending.lock().await.insert(
            peer_id,
            PendingChoice {
                url: "https://example.com".to_string(),
                title: "Example".to_string(),
            },
        );
        assert!(state.cancel(&peer_id).await);
        assert!(!state.cancel(&peer_id).await);

        // Test cancel with active operation
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        state
            .active_operations
            .lock()
            .await
            .insert(peer_id, handle.abort_handle());
        assert!(state.cancel(&peer_id).await);
        assert!(!state.cancel(&peer_id).await);
    }

    #[tokio::test]
    async fn test_temp_file_deleted_on_task_abort() {
        let path = std::env::temp_dir().join(format!(
            "rsdlp_abort_test_{}.tmp",
            std::process::id()
        ));
        std::fs::write(&path, b"abort payload").expect("create file");
        assert!(path.exists());

        let path_clone = path.clone();
        let handle = tokio::spawn(async move {
            let _guard = TempFileGuard::new(path_clone);
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        handle.abort();
        let _ = handle.await;

        assert!(!path.exists(), "temp file must be deleted when task is aborted");
    }

    #[test]
    fn test_temp_file_guard_deletion() {
        let path =
            std::env::temp_dir().join(format!("rsdlp_test_guard_{}.tmp", std::process::id()));
        std::fs::write(&path, b"test payload").expect("create test file");
        assert!(path.exists());

        {
            let _guard = TempFileGuard::new(path.clone());
        } // guard dropped here

        assert!(
            !path.exists(),
            "temporary file must be deleted when guard drops"
        );
    }

    #[test]
    fn test_config_from_env() {
        unsafe {
            env::remove_var("RSDLP_TG_API_ID");
            env::remove_var("RSDLP_TG_API_HASH");
            env::remove_var("RSDLP_TG_BOT_TOKEN");
            env::remove_var("TG_API_ID");
            env::remove_var("TG_API_HASH");
            env::remove_var("TG_BOT_TOKEN");
        }
        assert!(TelegramConfig::from_env().is_none());

        // Test prefixed RSDLP_* variables
        unsafe {
            env::set_var("RSDLP_TG_API_ID", "12345");
            env::set_var("RSDLP_TG_API_HASH", "abcdef");
            env::set_var("RSDLP_TG_BOT_TOKEN", "123:ABC");
        }
        let config = TelegramConfig::from_env().expect("config should parse");
        assert_eq!(config.api_id, 12345);
        assert_eq!(config.api_hash, "abcdef");
        assert_eq!(config.bot_token.as_deref(), Some("123:ABC"));

        // Clean up
        unsafe {
            env::remove_var("RSDLP_TG_API_ID");
            env::remove_var("RSDLP_TG_API_HASH");
            env::remove_var("RSDLP_TG_BOT_TOKEN");
        }
    }
}
