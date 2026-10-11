use std::sync::Arc;
use std::time::Instant;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageEntityKind};
use tracing::{error, info};

use crate::config::Config;
use crate::download::{download_and_send_audio, download_and_send_video};
use crate::helpers::{
    capitalize_first, extract_supported_url, get_video_duration, is_instagram_post,
    random_greeting, supported_url, user_facing_error,
};
use crate::types::{Cmd, Mode, Pending, PendingDownloads, UserModes, PENDING_TTL};

fn sender_id(msg: &Message) -> UserId {
    msg.from.as_ref().map(|u| u.id).unwrap_or(UserId(0))
}

/// The first supported link in a message: in its text or caption, or hidden
/// behind a text link ("[watch this](https://youtu.be/...)").
fn find_link(msg: &Message) -> Option<String> {
    let text = msg.text().or_else(|| msg.caption()).unwrap_or("");
    if let Some(url) = extract_supported_url(text) {
        return Some(url);
    }
    msg.entities()
        .or_else(|| msg.caption_entities())
        .into_iter()
        .flatten()
        .find_map(|e| match &e.kind {
            MessageEntityKind::TextLink { url } => supported_url(url.as_str()),
            _ => None,
        })
}

pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Cmd,
    modes: UserModes,
) -> ResponseResult<()> {
    let uid = sender_id(&msg);

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
    let is_private = msg.chat.is_private();

    let Some(url) = find_link(&msg) else {
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
    };

    let uid = sender_id(&msg);
    let mode = modes.get(&uid).map(|m| *m).unwrap_or_default();
    info!("Valid URL detected: mode={mode:?}, url={url}");

    // Long videos: offer audio instead before spending time on a big download.
    // Instagram posts skip this: they are mostly photos, and asking yt-dlp for
    // the length of a photo post only wastes a request.
    if matches!(mode, Mode::Video) && !is_instagram_post(&url) {
        if let Some(duration) = get_video_duration(&url, &config).await {
            if duration > config.long_video_secs {
                let mins = (duration / 60.0).ceil() as u64;
                // Drop prompts nobody answered so the map cannot grow forever.
                pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);
                pending.insert(
                    (msg.chat.id, uid),
                    Pending { url: url.clone(), created: Instant::now() },
                );

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
        Mode::Video => download_and_send_video(&bot, &msg, &url, &config).await,
        Mode::Audio => download_and_send_audio(&bot, &msg, &url, &config).await,
    };

    if let Err(e) = result {
        error!("Error processing {url}: {e}");
        if is_private {
            bot.send_message(msg.chat.id, user_facing_error(&e.to_string()))
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
        Some(d @ ("long_audio" | "long_continue")) => d,
        _ => return Ok(()),
    };

    let Some(reply_msg) = q.message.as_ref().and_then(|m| m.regular_message().cloned()) else {
        bot.answer_callback_query(q.id.clone()).await?;
        return Ok(());
    };
    let chat_id = reply_msg.chat.id;
    let is_private = reply_msg.chat.is_private();

    let url = match pending.remove(&(chat_id, q.from.id)) {
        Some((_, p)) if p.created.elapsed() < PENDING_TTL => p.url,
        _ => {
            bot.answer_callback_query(q.id.clone())
                .text("Линк је истекао, пошаљи поново.")
                .await?;
            return Ok(());
        }
    };

    // Acknowledge the button press and remove the keyboard from the prompt.
    bot.answer_callback_query(q.id.clone()).await?;
    let _ = bot.edit_message_reply_markup(chat_id, reply_msg.id).await;

    let result = if data == "long_audio" {
        bot.send_message(chat_id, "Преузимам као аудио...").await?;
        download_and_send_audio(&bot, &reply_msg, &url, &config).await
    } else {
        bot.send_message(chat_id, "Преузимам као видео...").await?;
        download_and_send_video(&bot, &reply_msg, &url, &config).await
    };

    if let Err(e) = result {
        error!("Error processing callback for {url}: {e}");
        // Same rule as plain messages: stay quiet in groups.
        if is_private {
            bot.send_message(chat_id, user_facing_error(&e.to_string())).await?;
        }
    }

    Ok(())
}
