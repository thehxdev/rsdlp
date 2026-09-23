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
use grammers_mtsender::SenderPool;
use grammers_session::defs::PeerId;
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

#[derive(Clone, Debug)]
pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: Option<String>,
    pub session_file: String,
}

impl TelegramConfig {
    pub fn from_env() -> Option<Self> {
        let api_id = env::var("TG_API_ID").ok()?.parse().ok()?;
        let api_hash = env::var("TG_API_HASH").ok()?;
        let bot_token = env::var("TG_BOT_TOKEN").ok().filter(|s| !s.trim().is_empty());
        let session_file =
            env::var("TG_SESSION_FILE").unwrap_or_else(|_| "rsdlp.session".to_string());

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
        println!("[telegram] Session not authorized. Signing in with bot token...");
        client.bot_sign_in(token, api_hash).await?;
        println!("[telegram] Bot token login successful!");
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

struct PendingChoice {
    url: String,
    title: String,
}

pub struct BotState {
    pending: Mutex<HashMap<PeerId, PendingChoice>>,
}

impl BotState {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
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
    println!(
        "[telegram] Userbot running as {} (id: {})",
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
                            eprintln!("[telegram] error handling message: {e}");
                        }
                    });
                }
            }
            _ => {}
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
            state.pending.lock().await.remove(&peer_id);
            message
                .respond(InputMessage::new().text("❌ Current operation cancelled."))
                .await?;
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
                    status_msg.edit(InputMessage::new().text(menu)).await?;
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

            let sort_str = ytdlp::build_sort(res, abr);
            let desc = match (res, abr) {
                (Some(r), _) => format!("{r}p"),
                (None, Some(a)) => format!("{a} kbps audio"),
                (None, None) => "best quality".to_string(),
            };

            let status_msg = message
                .respond(InputMessage::new().text(format!(
                    "⏳ Downloading {} ({desc})...",
                    pending.title
                )))
                .await?;

            let mut ytdlp = Ytdlp::new(
                &pending.url,
                if sort_str.is_empty() {
                    None
                } else {
                    Some(sort_str)
                },
            );

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

            let download_res = async {
                ytdlp.start_download()?;
                let mut temp_file = tokio::fs::File::create(&temp_path).await?;
                tokio::io::copy(&mut ytdlp, &mut temp_file).await?;
                temp_file.flush().await?;
                Ok::<(), std::io::Error>(())
            }
            .await;

            if let Err(e) = download_res {
                let _ = tokio::fs::remove_file(&temp_path).await;
                status_msg
                    .edit(InputMessage::new().text(format!("Download failed: {e}")))
                    .await?;
                return Ok(());
            }

            status_msg
                .edit(InputMessage::new().text("📤 Uploading to Telegram..."))
                .await?;

            let upload_res = client.upload_file(&temp_path).await;
            let _ = tokio::fs::remove_file(&temp_path).await;

            match upload_res {
                Ok(uploaded) => {
                    let caption = format!("🎬 {}\nQuality: {desc}", pending.title);
                    let input_file = InputMessage::new().file(uploaded).text(caption);
                    message.respond(input_file).await?;
                    let _ = status_msg.delete().await;
                }
                Err(e) => {
                    status_msg
                        .edit(InputMessage::new().text(format!("Upload failed: {e}")))
                        .await?;
                }
            }
        }
        TelegramAction::Unknown => {}
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
    fn test_config_from_env() {
        unsafe {
            env::remove_var("TG_API_ID");
            env::remove_var("TG_API_HASH");
            env::remove_var("TG_BOT_TOKEN");
        }
        assert!(TelegramConfig::from_env().is_none());

        unsafe {
            env::set_var("TG_API_ID", "12345");
            env::set_var("TG_API_HASH", "abcdef");
            env::set_var("TG_BOT_TOKEN", "123:ABC");
        }
        let config = TelegramConfig::from_env().expect("config should parse");
        assert_eq!(config.api_id, 12345);
        assert_eq!(config.api_hash, "abcdef");
        assert_eq!(config.bot_token.as_deref(), Some("123:ABC"));
        unsafe {
            env::remove_var("TG_API_ID");
            env::remove_var("TG_API_HASH");
            env::remove_var("TG_BOT_TOKEN");
        }
    }
}
