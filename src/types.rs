use std::sync::Arc;

use dashmap::DashMap;
use teloxide::types::UserId;
use teloxide::utils::command::BotCommands;

#[derive(Clone, Copy, Default, Debug)]
pub enum Mode {
    #[default]
    Video,
    Audio,
}

pub type UserModes = Arc<DashMap<UserId, Mode>>;
pub type PendingDownloads = Arc<DashMap<UserId, String>>;

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
