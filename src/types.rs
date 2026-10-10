use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use teloxide::types::{ChatId, Message, MessageId, UserId};
use teloxide::utils::command::BotCommands;

#[derive(Clone, Copy, Default, Debug)]
pub enum Mode {
    #[default]
    Video,
    Audio,
}

pub type UserModes = Arc<DashMap<UserId, Mode>>;

/// A long video waiting for the user to pick audio or video.
pub struct Pending {
    pub url: String,
    pub created: Instant,
}

/// Keyed by chat *and* user, so a second long link in another chat does not
/// overwrite the first, and only the person who sent a link can answer for it.
pub type PendingDownloads = Arc<DashMap<(ChatId, UserId), Pending>>;

/// How long the audio/video buttons stay valid.
pub const PENDING_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Cmd {
    #[command(description = "Покрени бота")]
    Start,
    #[command(description = "Преузми линк као видео")]
    Video,
    #[command(description = "Преузми линк као аудио")]
    Audio,
}

/// Where a reply goes: the chat, and in groups the message being answered.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub chat: ChatId,
    pub reply_to: Option<MessageId>,
}

impl Target {
    pub fn of(msg: &Message) -> Self {
        Self {
            chat: msg.chat.id,
            reply_to: (!msg.chat.is_private()).then_some(msg.id),
        }
    }
}
