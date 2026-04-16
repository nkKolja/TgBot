use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use tracing::{error, info};

use crate::config::Config;
use crate::download::{download_and_send_audio, download_and_send_video};
use crate::helpers::{capitalize_first, get_video_duration, is_supported_url, random_greeting};
use crate::types::{Cmd, Mode, PendingDownloads, UserModes};

pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Cmd,
    modes: UserModes,
) -> ResponseResult<()> {
    let uid = msg.from.as_ref().map(|u| u.id).unwrap_or(UserId(0));

    match cmd {
        Cmd::Start => {
            modes.insert(uid, Mode::Video);
            bot.send_message(
                msg.chat.id,
                format!(
                    "Ђе с' {}. Пошаљи линк да ти пошаљем видео.\n\
                     /video - видео мод\n\
                     /audio - аудио мод",
                    random_greeting()
                ),
            )
            .await?;
        }
        Cmd::Video => {
            modes.insert(uid, Mode::Video);
            bot.send_message(msg.chat.id, "Мод је подешен на видео. Сад само пошаљи линк.")
                .await?;
        }
        Cmd::Audio => {
            modes.insert(uid, Mode::Audio);
            bot.send_message(msg.chat.id, "Мод је подешен на аудио. Сад само пошаљи линк.")
                .await?;
        }
    }
    Ok(())
}

pub async fn handle_message(
    bot: Bot,
    msg: Message,
    modes: UserModes,
    pending: PendingDownloads,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let text = msg.text().unwrap_or("").trim();
    let is_private = msg.chat.is_private();

    if !is_supported_url(text) {
        info!("Non-link message received");
        if is_private {
            bot.send_message(
                msg.chat.id,
                format!(
                    "{}, баци линк.\n/video за видео\n/audio за само звук",
                    capitalize_first(random_greeting())
                ),
            )
            .await?;
        }
        return Ok(());
    }

    let uid = msg.from.as_ref().map(|u| u.id).unwrap_or(UserId(0));
    let mode = modes.get(&uid).map(|m| *m).unwrap_or_default();
    info!("Valid URL detected: mode={mode:?}, url={text}");

    // Duration check: when in Video mode, warn if video > 5 minutes
    if matches!(mode, Mode::Video) {
        if let Some(duration) = get_video_duration(text, &config).await {
            if duration > 300.0 {
                let mins = (duration / 60.0).ceil() as u64;
                pending.insert(uid, text.to_owned());

                let keyboard = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("🎵 Аудио", "long_audio"),
                    InlineKeyboardButton::callback("▶️ Настави", "long_continue"),
                ]]);

                bot.send_message(
                    msg.chat.id,
                    format!(
                        "Видео траје ~{mins} мин. Фајл може бити превелик.\n\
                         Изабери: аудио или настави са видеом?"
                    ),
                )
                .reply_markup(keyboard)
                .await?;

                return Ok(());
            }
        }
    }

    let result = match mode {
        Mode::Video => download_and_send_video(&bot, &msg, text, &config).await,
        Mode::Audio => download_and_send_audio(&bot, &msg, text, &config).await,
    };

    if let Err(e) = result {
        error!("Error processing {text}: {e}");
        if is_private {
            bot.send_message(msg.chat.id, format!("Error: {e}"))
                .await?;
        }
    }

    Ok(())
}

pub async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    pending: PendingDownloads,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    if data != "long_audio" && data != "long_continue" {
        return Ok(());
    }

    let uid = q.from.id;
    let url = match pending.remove(&uid) {
        Some((_, url)) => url,
        None => {
            bot.answer_callback_query(&q.id)
                .text("Линк је истекао, пошаљи поново.")
                .await?;
            return Ok(());
        }
    };

    // Acknowledge the button press
    bot.answer_callback_query(&q.id).await?;

    // Remove the inline keyboard from the prompt message
    if let Some(msg) = &q.message {
        if let Some(msg) = msg.regular_message() {
            let _ = bot
                .edit_message_reply_markup(msg.chat.id, msg.id)
                .await;
        }
    }

    // Determine the original message to reply to (find the message the prompt replied to, or use the prompt itself)
    let reply_msg = q
        .message
        .as_ref()
        .and_then(|m| m.regular_message().cloned());

    let chat_id = match &reply_msg {
        Some(m) => m.chat.id,
        None => return Ok(()),
    };

    let result = match data {
        "long_audio" => {
            bot.send_message(chat_id, "Преузимам као аудио...").await?;
            if let Some(m) = &reply_msg {
                download_and_send_audio(&bot, m, &url, &config).await
            } else {
                return Ok(());
            }
        }
        "long_continue" => {
            bot.send_message(chat_id, "Преузимам као видео...").await?;
            if let Some(m) = &reply_msg {
                download_and_send_video(&bot, m, &url, &config).await
            } else {
                return Ok(());
            }
        }
        _ => return Ok(()),
    };

    if let Err(e) = result {
        error!("Error processing callback for {url}: {e}");
        bot.send_message(chat_id, format!("Error: {e}")).await?;
    }

    // Mode remains Video — no change needed
    Ok(())
}
