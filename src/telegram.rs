use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use grammers_client::client::{Client, UpdatesConfiguration};
use grammers_client::types::update;
use grammers_client::types::{Attribute, InputMessage, Update};
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
        if let Some(parent) = self.path.parent() {
            if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("rsdlp_job_") {
                    let _ = std::fs::remove_dir_all(parent);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: String,
    pub session_file: String,
}

impl TelegramConfig {
    #[allow(dead_code)]
    pub fn from_env() -> Option<Self> {
        let get_var = |key: &str| {
            env::var(format!("RSDLP_{key}"))
                .ok()
                .or_else(|| env::var(key).ok())
        };

        let api_id = get_var("TG_API_ID")?.parse().ok()?;
        let api_hash = get_var("TG_API_HASH")?;
        let bot_token = get_var("TG_BOT_TOKEN")?.trim().to_string();
        if bot_token.is_empty() {
            return None;
        }
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

pub struct PendingChoice {
    pub url: String,
    pub title: String,
}

pub struct BotState {
    pub pending: Mutex<HashMap<PeerId, PendingChoice>>,
    pub active_operations: Mutex<HashMap<PeerId, tokio::task::AbortHandle>>,
    pub staging_dir: PathBuf,
    pub db: Option<crate::db::Database>,
    seen_messages: Mutex<(VecDeque<(PeerId, i32)>, HashSet<(PeerId, i32)>)>,
    seen_callbacks: Mutex<(VecDeque<i64>, HashSet<i64>)>,
}

impl BotState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            active_operations: Mutex::new(HashMap::new()),
            staging_dir: std::env::temp_dir(),
            db: None,
            seen_messages: Mutex::new((VecDeque::new(), HashSet::new())),
            seen_callbacks: Mutex::new((VecDeque::new(), HashSet::new())),
        }
    }

    pub fn new_with_staging(staging_dir: PathBuf) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            active_operations: Mutex::new(HashMap::new()),
            staging_dir,
            db: None,
            seen_messages: Mutex::new((VecDeque::new(), HashSet::new())),
            seen_callbacks: Mutex::new((VecDeque::new(), HashSet::new())),
        }
    }

    pub fn new_with_db(staging_dir: PathBuf, db: crate::db::Database) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            active_operations: Mutex::new(HashMap::new()),
            staging_dir,
            db: Some(db),
            seen_messages: Mutex::new((VecDeque::new(), HashSet::new())),
            seen_callbacks: Mutex::new((VecDeque::new(), HashSet::new())),
        }
    }

    pub async fn mark_message_seen(&self, peer_id: PeerId, msg_id: i32) -> bool {
        let mut guard = self.seen_messages.lock().await;
        let key = (peer_id, msg_id);
        if guard.1.contains(&key) {
            return false;
        }
        if guard.0.len() >= 1000 {
            if let Some(old) = guard.0.pop_front() {
                guard.1.remove(&old);
            }
        }
        guard.0.push_back(key);
        guard.1.insert(key);
        true
    }

    pub async fn mark_callback_seen(&self, query_id: i64) -> bool {
        if query_id == 0 {
            return true;
        }
        let mut guard = self.seen_callbacks.lock().await;
        if guard.1.contains(&query_id) {
            return false;
        }
        if guard.0.len() >= 1000 {
            if let Some(old) = guard.0.pop_front() {
                guard.1.remove(&old);
            }
        }
        guard.0.push_back(query_id);
        guard.1.insert(query_id);
        true
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

#[derive(serde::Serialize, Clone, Debug)]
pub struct TelegramStatus {
    pub configured: bool,
    pub running: bool,
    pub bot_username: Option<String>,
    pub bot_name: Option<String>,
    pub session_file: String,
    pub api_id: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct BotInfo {
    pub bot_username: Option<String>,
    pub bot_name: Option<String>,
    pub api_id: i32,
}

#[derive(Clone)]
pub struct TelegramManager {
    db: crate::db::Database,
    session_file: String,
    staging_dir: PathBuf,
    bot_task: Arc<tokio::sync::Mutex<Option<tokio::task::AbortHandle>>>,
    bot_info: Arc<tokio::sync::RwLock<Option<BotInfo>>>,
}

impl TelegramManager {
    pub fn new(db: crate::db::Database, session_file: String, staging_dir: PathBuf) -> Self {
        Self {
            db,
            session_file,
            staging_dir,
            bot_task: Arc::new(tokio::sync::Mutex::new(None)),
            bot_info: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    pub async fn get_config(&self) -> Option<TelegramConfig> {
        let api_id: Option<i32> = self
            .db
            .get_config("tg_api_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                std::env::var("RSDLP_TG_API_ID")
                    .or_else(|_| std::env::var("TG_API_ID"))
                    .ok()
                    .and_then(|s| s.parse().ok())
            });
        let api_hash = self
            .db
            .get_config("tg_api_hash")
            .ok()
            .flatten()
            .or_else(|| {
                std::env::var("RSDLP_TG_API_HASH")
                    .or_else(|_| std::env::var("TG_API_HASH"))
                    .ok()
            });
        let bot_token = self
            .db
            .get_config("tg_bot_token")
            .ok()
            .flatten()
            .or_else(|| {
                std::env::var("RSDLP_TG_BOT_TOKEN")
                    .or_else(|_| std::env::var("TG_BOT_TOKEN"))
                    .ok()
            })
            .filter(|s| !s.trim().is_empty());

        let api_id = api_id?;
        let api_hash = api_hash?;
        let bot_token = bot_token?;

        Some(TelegramConfig {
            api_id,
            api_hash,
            bot_token,
            session_file: self.session_file.clone(),
        })
    }

    pub async fn get_status(&self) -> TelegramStatus {
        let info = self.bot_info.read().await.clone();
        if let Some(info) = info {
            TelegramStatus {
                configured: true,
                running: true,
                bot_username: info.bot_username,
                bot_name: info.bot_name,
                session_file: self.session_file.clone(),
                api_id: Some(info.api_id),
            }
        } else if let Some(cfg) = self.get_config().await {
            TelegramStatus {
                configured: true,
                running: false,
                bot_username: None,
                bot_name: None,
                session_file: self.session_file.clone(),
                api_id: Some(cfg.api_id),
            }
        } else {
            TelegramStatus {
                configured: false,
                running: false,
                bot_username: None,
                bot_name: None,
                session_file: self.session_file.clone(),
                api_id: None,
            }
        }
    }

    pub async fn start_bot(&self) -> Result<(), String> {
        let cfg = self
            .get_config()
            .await
            .ok_or("Telegram bot token, api_id, or api_hash missing")?;
        let mut task_guard = self.bot_task.lock().await;
        if let Some(old) = task_guard.take() {
            old.abort();
        }
        *self.bot_info.write().await = None;

        let staging = self.staging_dir.clone();
        let db_clone = self.db.clone();
        let info_clone = Arc::clone(&self.bot_info);
        let handle = tokio::spawn(async move {
            if let Err(e) = run_bot_service(cfg, staging, Some(db_clone), info_clone).await {
                tracing::error!("[telegram] bot error: {e}");
            }
        });

        *task_guard = Some(handle.abort_handle());
        tracing::info!("[telegram] Telegram bot task started");
        Ok(())
    }

    pub async fn stop_bot(&self) {
        let mut task_guard = self.bot_task.lock().await;
        if let Some(handle) = task_guard.take() {
            handle.abort();
            *self.bot_info.write().await = None;
            tracing::info!("[telegram] Telegram bot task stopped");
        }
    }

    pub async fn disconnect(&self) -> Result<(), String> {
        self.stop_bot().await;
        if std::path::Path::new(&self.session_file).exists() {
            let _ = std::fs::remove_file(&self.session_file);
        }
        let _ = self.db.delete_config("tg_bot_token");
        Ok(())
    }

    pub async fn init_from_db(&self) {
        if self.get_config().await.is_some() {
            tracing::info!("[telegram] Telegram bot configured. Starting bot runner...");
            let _ = self.start_bot().await;
        } else {
            tracing::info!("[telegram] Telegram bot not configured. Running web-only mode.");
        }
    }
}

struct BotInfoGuard(Arc<tokio::sync::RwLock<Option<BotInfo>>>);
impl Drop for BotInfoGuard {
    fn drop(&mut self) {
        if let Ok(mut lock) = self.0.try_write() {
            *lock = None;
        }
    }
}

#[allow(dead_code)]
pub async fn run_bot(
    config: TelegramConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bot_info = Arc::new(tokio::sync::RwLock::new(None));
    run_bot_service(config, std::env::temp_dir(), None, bot_info).await
}

struct TaskAbortOnDrop(tokio::task::AbortHandle);
impl Drop for TaskAbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn run_bot_service(
    config: TelegramConfig,
    staging_dir: PathBuf,
    db: Option<crate::db::Database>,
    bot_info: Arc<tokio::sync::RwLock<Option<BotInfo>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _guard = BotInfoGuard(Arc::clone(&bot_info));

    let session = Arc::new(SqliteSession::open(&config.session_file)?);
    let pool = SenderPool::new(Arc::clone(&session), config.api_id);
    let client = Client::new(&pool);
    let pool_task = tokio::spawn(pool.runner.run());
    let _pool_task_guard = TaskAbortOnDrop(pool_task.abort_handle());

    if !client.is_authorized().await? {
        tracing::info!("[telegram] Authorizing bot with token...");
        client.bot_sign_in(&config.bot_token, &config.api_hash).await?;
        tracing::info!("[telegram] Bot token login successful");
    }

    let me = client.get_me().await?;
    let me_peer_id = PeerId::user(me.raw.id());
    let bot_username = me.username().map(String::from);
    let bot_name = me.first_name().map(String::from);
    tracing::info!(
        "[telegram] bot running as @{} ({}) (id: {})",
        bot_username.as_deref().unwrap_or("unknown"),
        bot_name.as_deref().unwrap_or("Bot"),
        me.raw.id()
    );

    *bot_info.write().await = Some(BotInfo {
        bot_username,
        bot_name,
        api_id: config.api_id,
    });

    let state = match db {
        Some(database) => Arc::new(BotState::new_with_db(staging_dir, database)),
        None => Arc::new(BotState::new_with_staging(staging_dir)),
    };
    let mut update_stream = client.stream_updates(
        pool.updates,
        UpdatesConfiguration::default(),
    );

    while let Ok(update) = update_stream.next().await {
        update_stream.sync_update_state();
        match update {
            Update::NewMessage(message) => {
                let peer_id = message.peer_id();
                let is_saved_messages = peer_id == me_peer_id;
                // Process if incoming message, or user typing to self in Saved Messages
                if !message.outgoing() || is_saved_messages {
                    if !state.mark_message_seen(peer_id, message.id()).await {
                        tracing::debug!(
                            "[telegram] ignoring duplicate message id {} for peer {:?}",
                            message.id(),
                            peer_id
                        );
                        continue;
                    }
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
                let qid = match &query.raw {
                    grammers_client::grammers_tl_types::enums::Update::BotCallbackQuery(u) => {
                        u.query_id
                    }
                    grammers_client::grammers_tl_types::enums::Update::InlineBotCallbackQuery(
                        u,
                    ) => u.query_id,
                    _ => 0,
                };
                if !state.mark_callback_seen(qid).await {
                    tracing::debug!("[telegram] ignoring duplicate callback query {qid}");
                    continue;
                }
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
            let staging_dir = state.staging_dir.clone();

            let join_handle = tokio::spawn(async move {
                let res = execute_download_and_upload(
                    &client_clone,
                    &peer,
                    pending,
                    res,
                    abr,
                    &desc,
                    msg_id,
                    &staging_dir,
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
            if let Some(db) = &state.db {
                if let Ok(Some(domain)) = db.check_url_blacklisted(&url) {
                    let notice = format!(
                        "⚠️ Provider Disabled\n\nThis media provider ('{domain}') is currently disabled because processing its streams is a heavy operation that requires intensive CPU and server resources."
                    );
                    message.respond(InputMessage::new().text(notice)).await?;
                    return Ok(());
                }
            }

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
            let staging_dir = state.staging_dir.clone();

            let join_handle = tokio::spawn(async move {
                let res = execute_download_and_upload(
                    &client_clone,
                    peer_ref,
                    pending,
                    res,
                    abr,
                    &desc,
                    None,
                    &staging_dir,
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

#[derive(Debug, Clone, PartialEq)]
pub struct MediaMetadata {
    pub is_video: bool,
    pub is_audio: bool,
    pub duration_secs: u64,
    pub width: i32,
    pub height: i32,
    pub mime_type: String,
    pub title: Option<String>,
    pub artist: Option<String>,
}

pub async fn probe_media(
    path: &std::path::Path,
    info: Option<&serde_json::Value>,
    res: Option<u64>,
    abr: Option<u64>,
) -> MediaMetadata {
    // 1. Try ffprobe for container and stream properties
    if let Ok(output) = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            path.to_str().unwrap_or_default(),
        ])
        .output()
        .await
    {
        if output.status.success() {
            if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                let streams = parsed.get("streams").and_then(|v| v.as_array());
                let format = parsed.get("format");

                let mut is_video = false;
                let mut is_audio = false;
                let mut width = 0;
                let mut height = 0;
                let mut duration_secs = 0u64;

                if let Some(streams) = streams {
                    for s in streams {
                        let codec_type = s.get("codec_type").and_then(|v| v.as_str());
                        if codec_type == Some("video") {
                            is_video = true;
                            if width == 0 {
                                width = s
                                    .get("width")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0) as i32;
                            }
                            if height == 0 {
                                height = s
                                    .get("height")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0) as i32;
                            }
                            if duration_secs == 0 {
                                if let Some(d_str) =
                                    s.get("duration").and_then(|v| v.as_str())
                                {
                                    if let Ok(d_float) = d_str.parse::<f64>() {
                                        duration_secs = d_float as u64;
                                    }
                                }
                            }
                        } else if codec_type == Some("audio") {
                            is_audio = true;
                        }
                    }
                }

                if duration_secs == 0 {
                    if let Some(d_str) =
                        format.and_then(|f| f.get("duration")).and_then(|v| v.as_str())
                    {
                        if let Ok(d_float) = d_str.parse::<f64>() {
                            duration_secs = d_float as u64;
                        }
                    }
                }

                let title = format
                    .and_then(|f| f.get("tags"))
                    .and_then(|t| t.get("title"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let artist = format
                    .and_then(|f| f.get("tags"))
                    .and_then(|t| t.get("artist"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let mime_type = match ext.as_str() {
                    "mp4" | "m4v" => "video/mp4",
                    "webm" => {
                        if is_video {
                            "video/webm"
                        } else {
                            "audio/webm"
                        }
                    }
                    "mkv" => "video/x-matroska",
                    "mp3" => "audio/mpeg",
                    "m4a" | "aac" => "audio/mp4",
                    "ogg" | "opus" => "audio/ogg",
                    "flac" => "audio/flac",
                    _ => {
                        if is_video {
                            "video/mp4"
                        } else if is_audio {
                            "audio/mp4"
                        } else {
                            "application/octet-stream"
                        }
                    }
                }
                .to_string();

                return MediaMetadata {
                    is_video,
                    is_audio: !is_video && is_audio,
                    duration_secs,
                    width,
                    height,
                    mime_type,
                    title,
                    artist,
                };
            }
        }
    }

    // 2. Fallback to info JSON from yt-dlp
    let is_audio_only = (res.is_none() && abr.is_some())
        || info
            .and_then(|i| i.get("vcodec"))
            .and_then(|v| v.as_str())
            .map(|vc| vc == "none")
            .unwrap_or(false);

    let is_video = !is_audio_only;
    let is_audio = is_audio_only;

    let duration_secs = info
        .and_then(|i| i.get("duration"))
        .and_then(|v| v.as_f64())
        .map(|d| d as u64)
        .unwrap_or(0);

    let width = info
        .and_then(|i| i.get("width"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let height = info
        .and_then(|i| i.get("height"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let title = info
        .and_then(|i| i.get("title"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let artist = info
        .and_then(|i| i.get("artist").or_else(|| i.get("uploader")))
        .and_then(|v| v.as_str())
        .map(String::from);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime_type = match ext.as_str() {
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "webm" => {
            if is_video {
                "video/webm"
            } else {
                "audio/webm"
            }
        }
        _ => {
            if is_video {
                "video/mp4"
            } else {
                "audio/mp4"
            }
        }
    }
    .to_string();

    MediaMetadata {
        is_video,
        is_audio,
        duration_secs,
        width,
        height,
        mime_type,
        title,
        artist,
    }
}

async fn ensure_faststart(path: &std::path::Path) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext != "mp4" && ext != "m4v" && ext != "mov" {
        return;
    }
    let temp_faststart = path.with_extension("faststart_tmp.mp4");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            path.to_str().unwrap_or_default(),
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            temp_faststart.to_str().unwrap_or_default(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;

    if let Ok(st) = status {
        if st.success() && temp_faststart.exists() {
            let _ = tokio::fs::rename(&temp_faststart, path).await;
        } else {
            let _ = tokio::fs::remove_file(&temp_faststart).await;
        }
    }
}

pub fn build_media_message(
    uploaded: grammers_client::types::media::Uploaded,
    caption: String,
    meta: &MediaMetadata,
) -> InputMessage {
    let mut input_msg = InputMessage::new()
        .mime_type(&meta.mime_type)
        .document(uploaded)
        .text(caption);

    if meta.is_video {
        input_msg = input_msg.attribute(Attribute::Video {
            round_message: false,
            supports_streaming: true,
            duration: std::time::Duration::from_secs(meta.duration_secs),
            w: meta.width,
            h: meta.height,
        });
    } else if meta.is_audio {
        input_msg = input_msg.attribute(Attribute::Audio {
            duration: std::time::Duration::from_secs(meta.duration_secs),
            title: meta.title.clone(),
            performer: meta.artist.clone(),
        });
    }

    input_msg
}

async fn execute_download_and_upload<P: Into<PeerRef>>(
    client: &Client,
    peer: P,
    pending: PendingChoice,
    res: Option<u64>,
    abr: Option<u64>,
    desc: &str,
    status_msg_id: Option<i32>,
    staging_dir: &std::path::Path,
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
        .text(format!("⏳ [1/2] Downloading {} ({desc}) to server disk...", pending.title))
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

    let (info_val, filename) = match ytdlp.get_info().await {
        Ok(Some(info)) => {
            let fn_str = ytdlp::extract_filename(&info);
            (Some(info), fn_str)
        }
        _ => (None, "video.mp4".to_string()),
    };

    let id = DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let safe_filename = filename.replace(['/', '\\', '\0'], "_");
    let job_dir = staging_dir.join(format!("rsdlp_job_{}_{}", std::process::id(), id));
    let _ = tokio::fs::create_dir_all(&job_dir).await;
    let temp_path = job_dir.join(&safe_filename);
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
        let err_text = if e.kind() == std::io::ErrorKind::StorageFull {
            "Download failed: Server storage full. Contact administrator.".to_string()
        } else {
            format!("Download failed: {e}")
        };
        let _ = client
            .edit_message(
                peer_ref,
                status_msg_id,
                InputMessage::new().text(err_text),
            )
            .await;
        return Ok(());
    }

    ensure_faststart(guard.path()).await;
    let meta = probe_media(guard.path(), info_val.as_ref(), res, abr).await;

    let uploading_msg = InputMessage::new()
        .text(format!("📤 [2/2] Uploading {} to Telegram...", pending.title))
        .reply_markup(&cancel_markup());
    let _ = client
        .edit_message(peer_ref, status_msg_id, uploading_msg)
        .await;

    let upload_res = client.upload_file(guard.path()).await;

    match upload_res {
        Ok(uploaded) => {
            let caption = format!("🎬 {}\nQuality: {desc}", pending.title);
            let input_file = build_media_message(uploaded, caption, &meta);
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
    async fn test_message_and_callback_deduplication() {
        let state = BotState::new();
        let peer_id = PeerId::user(12345);

        // First time message 42 is seen -> returns true
        assert!(state.mark_message_seen(peer_id, 42).await);

        // Duplicate delivery of message 42 -> returns false
        assert!(!state.mark_message_seen(peer_id, 42).await);

        // Different message ID 43 -> returns true
        assert!(state.mark_message_seen(peer_id, 43).await);

        // First time callback query 999 -> returns true
        assert!(state.mark_callback_seen(999).await);

        // Duplicate callback query 999 -> returns false
        assert!(!state.mark_callback_seen(999).await);

        // Different callback query 1000 -> returns true
        assert!(state.mark_callback_seen(1000).await);
    }

    #[tokio::test]
    async fn test_probe_media_fallback() {
        let dummy_path = std::path::Path::new("dummy_video.mp4");
        let video_info = serde_json::json!({
            "duration": 120.5,
            "width": 1920,
            "height": 1080,
            "title": "Sample Video",
            "uploader": "Test Channel"
        });
        let meta = probe_media(dummy_path, Some(&video_info), Some(1080), None).await;
        assert!(meta.is_video);
        assert!(!meta.is_audio);
        assert_eq!(meta.duration_secs, 120);
        assert_eq!(meta.width, 1920);
        assert_eq!(meta.height, 1080);
        assert_eq!(meta.mime_type, "video/mp4");
        assert_eq!(meta.title.as_deref(), Some("Sample Video"));
        assert_eq!(meta.artist.as_deref(), Some("Test Channel"));

        let dummy_audio_path = std::path::Path::new("dummy_song.mp3");
        let audio_info = serde_json::json!({
            "duration": 45.0,
            "vcodec": "none",
            "title": "Sample Song",
            "artist": "Sample Artist"
        });
        let audio_meta = probe_media(dummy_audio_path, Some(&audio_info), None, Some(128)).await;
        assert!(!audio_meta.is_video);
        assert!(audio_meta.is_audio);
        assert_eq!(audio_meta.duration_secs, 45);
        assert_eq!(audio_meta.mime_type, "audio/mpeg");
        assert_eq!(audio_meta.title.as_deref(), Some("Sample Song"));
        assert_eq!(audio_meta.artist.as_deref(), Some("Sample Artist"));
    }

    #[tokio::test]
    async fn test_probe_media_ffprobe_and_faststart() {
        let test_file = std::env::temp_dir().join(format!("rsdlp_test_probe_{}.mp4", std::process::id()));
        let status = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=640x360:rate=30",
                "-c:v",
                "libx264",
                test_file.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        if let Ok(st) = status {
            if st.success() {
                ensure_faststart(&test_file).await;
                let meta = probe_media(&test_file, None, None, None).await;
                assert!(meta.is_video);
                assert!(!meta.is_audio);
                assert_eq!(meta.width, 640);
                assert_eq!(meta.height, 360);
                assert_eq!(meta.mime_type, "video/mp4");
                let _ = std::fs::remove_file(&test_file);
            }
        }
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
        assert_eq!(config.bot_token, "123:ABC");

        // Clean up
        unsafe {
            env::remove_var("RSDLP_TG_API_ID");
            env::remove_var("RSDLP_TG_API_HASH");
            env::remove_var("RSDLP_TG_BOT_TOKEN");
        }
    }
}
