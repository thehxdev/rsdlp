use crate::ytdlp::Qualities;

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
    let mut msg = format!("🎬 <b>{title}</b>\n\nChoose quality to download:\n\n");

    if !qualities.video.is_empty() {
        msg.push_str("📹 <b>Video:</b>\n");
        for &res in qualities.video.iter().rev() {
            msg.push_str(&format!("  /dl_{res} — {res}p\n"));
        }
        msg.push('\n');
    }

    if !qualities.audio.is_empty() {
        msg.push_str("🎵 <b>Audio only:</b>\n");
        for &abr in qualities.audio.iter().rev() {
            msg.push_str(&format!("  /dl_audio_{abr} — {abr} kbps\n"));
        }
        msg.push('\n');
    }

    msg.push_str("⚡ /dl_best — Best available\n");
    msg.push_str("❌ /cancel — Cancel");
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_message_action() {
        assert_eq!(
            parse_message_action("/start"),
            TelegramAction::Help
        );
        assert_eq!(
            parse_message_action("/cancel"),
            TelegramAction::Cancel
        );
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
            TelegramAction::SelectQuality { res: Some(720), abr: None }
        );
        assert_eq!(
            parse_message_action("/dl 1080"),
            TelegramAction::SelectQuality { res: Some(1080), abr: None }
        );
        assert_eq!(
            parse_message_action("/dl_audio_320"),
            TelegramAction::SelectQuality { res: None, abr: Some(320) }
        );
        assert_eq!(
            parse_message_action("/dl_best"),
            TelegramAction::SelectQuality { res: None, abr: None }
        );
    }

    #[test]
    fn test_format_qualities_menu() {
        let qualities = Qualities {
            video: vec![360, 720, 1080],
            audio: vec![128, 320],
        };
        let menu = format_qualities_menu("Test Title", &qualities);
        assert!(menu.contains("🎬 <b>Test Title</b>"));
        assert!(menu.contains("/dl_1080 — 1080p"));
        assert!(menu.contains("/dl_720 — 720p"));
        assert!(menu.contains("/dl_audio_320 — 320 kbps"));
        assert!(menu.contains("/dl_best"));
        assert!(menu.contains("/cancel"));
    }
}
