use std::sync::Arc;

use teloxide::prelude::*;
use tracing::{error, info};

use crate::config::Config;
use crate::download::{download_and_send_audio, download_and_send_video};
use crate::helpers::{capitalize_first, is_supported_url, random_greeting};
use crate::types::{Cmd, Mode, UserModes};

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
