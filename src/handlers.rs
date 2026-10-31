use std::sync::Arc;
use std::time::Instant;

use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, MessageEntityKind, ReplyParameters,
};
use tracing::{debug, error, info};

use crate::config::Config;
use crate::download::{download_and_send_audio, download_and_send_video};
use crate::helpers::{
    capitalize_first, extract_supported_url, get_video_duration, is_audio_site, prefers_gallery,
    random_greeting, supported_url, user_facing_error,
};
use crate::types::{Cmd, Mode, Pending, PendingDownloads, Target, UserModes, PENDING_TTL};

fn sender_id(msg: &Message) -> UserId {
    msg.from.as_ref().map(|u| u.id).unwrap_or(UserId(0))
}

/// The person who sent a message, or `None` when it was sent anonymously: by
/// an admin posting as the group or by a channel, where `from` is one of
/// Telegram's stand-in bots and the person behind it cannot be told.
fn known_sender(msg: &Message) -> Option<UserId> {
    if msg.sender_chat.is_some() {
        return None;
    }
    msg.from.as_ref().filter(|u| !u.is_bot).map(|u| u.id)
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

/// Whether a message links anywhere at all, supported or not.
fn has_any_link(msg: &Message) -> bool {
    msg.entities()
        .or_else(|| msg.caption_entities())
        .into_iter()
        .flatten()
        .any(|e| matches!(e.kind, MessageEntityKind::Url | MessageEntityKind::TextLink { .. }))
}

/// What to do about a video longer than LONG_VIDEO_SECS.
#[derive(Clone, Copy)]
enum LongVideo {
    /// Offer audio first.
    Ask,
    /// The user asked for video outright (`/video <link>`).
    Download,
}

pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Cmd,
    modes: UserModes,
    pending: PendingDownloads,
    config: Arc<Config>,
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
        Cmd::Video | Cmd::Audio => {
            let mode = if matches!(cmd, Cmd::Audio) { Mode::Audio } else { Mode::Video };
            // "/audio <link>", or /audio in reply to a message with a link,
            // fetches that one link this way and leaves the saved mode alone.
            // The command parser drops the arguments, so the link used to be
            // lost.
            let link = find_link(&msg).or_else(|| msg.reply_to_message().and_then(find_link));
            if let Some(url) = link {
                return process_link(&bot, &msg, url, mode, LongVideo::Download, &pending, &config)
                    .await;
            }
            modes.insert(uid, mode);
            let reply = match mode {
                Mode::Video => "Мод је подешен на видео. Сад само пошаљи линк.",
                Mode::Audio => "Мод је подешен на аудио. Сад само пошаљи линк.",
            };
            bot.send_message(msg.chat.id, reply).await?;
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
    let Some(url) = find_link(&msg) else {
        debug!("Message without a supported link");
        if msg.chat.is_private() {
            let reply = if has_any_link(&msg) {
                "Тај линк не умем да преузмем. Пошаљи линк до видеа или објаве, \
                 не до профила или канала."
                    .to_string()
            } else {
                format!(
                    "{}, баци линк.\n/video за видео\n/audio за само звук",
                    capitalize_first(random_greeting())
                )
            };
            bot.send_message(msg.chat.id, reply).await?;
        }
        return Ok(());
    };

    let mode = modes.get(&sender_id(&msg)).map(|m| *m).unwrap_or_default();
    process_link(&bot, &msg, url, mode, LongVideo::Ask, &pending, &config).await
}

/// Download one link and send what it holds, or first ask about audio when
/// the video is long.
async fn process_link(
    bot: &Bot,
    msg: &Message,
    url: String,
    mode: Mode,
    long_video: LongVideo,
    pending: &PendingDownloads,
    config: &Config,
) -> ResponseResult<()> {
    let target = Target::of(msg);
    // SoundCloud and the like have nothing to send but sound.
    let mode = if is_audio_site(&url) { Mode::Audio } else { mode };
    info!("Valid URL detected: mode={mode:?}, url={url}");

    // Long videos: offer audio instead before spending time on a big download.
    // Instagram posts and thread links skip this: Instagram posts are mostly
    // photos, yt-dlp does not know thread links, and asking it for their
    // length only wastes a request.
    if matches!((mode, long_video), (Mode::Video, LongVideo::Ask)) && !prefers_gallery(&url) {
        if let Some(duration) = get_video_duration(&url, config).await {
            if duration > config.long_video_secs {
                return ask_audio_or_video(bot, msg, target, url, duration, pending).await;
            }
        }
    }

    let result = match mode {
        Mode::Video => download_and_send_video(bot, target, &url, config).await,
        Mode::Audio => download_and_send_audio(bot, target, &url, config).await,
    };

    if let Err(e) = result {
        error!("Error processing {url}: {e}");
        if msg.chat.is_private() {
            bot.send_message(msg.chat.id, user_facing_error(&e.to_string())).await?;
        }
    }
    Ok(())
}

/// Ask whether a long video should come as audio. The link waits in `pending`
/// under the question's message until its sender answers.
async fn ask_audio_or_video(
    bot: &Bot,
    msg: &Message,
    target: Target,
    url: String,
    duration: f64,
    pending: &PendingDownloads,
) -> ResponseResult<()> {
    let mins = (duration / 60.0).ceil() as u64;
    // Drop prompts nobody answered so the map cannot grow forever.
    pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);

    let keyboard = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("🎵 Аудио", "long_audio"),
        InlineKeyboardButton::callback("▶️ Настави", "long_continue"),
    ]]);
    let mut question = bot
        .send_message(
            msg.chat.id,
            format!(
                "Видео траје ~{mins} мин. Фајл може бити превелик.\n\
                 Изабери: аудио или настави са видеом?"
            ),
        )
        .reply_markup(keyboard);
    // In a group, show whose link the question is about.
    if let Some(id) = target.reply_to {
        question = question.reply_parameters(ReplyParameters::new(id));
    }
    let question = question.await?;

    pending.insert(
        (question.chat.id, question.id),
        Pending { url, user: known_sender(msg), target, created: Instant::now() },
    );
    Ok(())
}

pub async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    pending: PendingDownloads,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let mode = match q.data.as_deref() {
        Some("long_audio") => Mode::Audio,
        Some("long_continue") => Mode::Video,
        _ => {
            bot.answer_callback_query(q.id.clone()).await?;
            return Ok(());
        }
    };

    let Some(question) = q.message.as_ref().and_then(|m| m.regular_message()) else {
        bot.answer_callback_query(q.id.clone()).await?;
        return Ok(());
    };
    let key = (question.chat.id, question.id);

    // Only whoever sent the link decides; the buttons stay up for them.
    if pending.get(&key).is_some_and(|p| p.user.is_some_and(|user| user != q.from.id)) {
        bot.answer_callback_query(q.id.clone())
            .text("Ово питање није за тебе.")
            .await?;
        return Ok(());
    }

    let job = match pending.remove(&key) {
        Some((_, p)) if p.created.elapsed() < PENDING_TTL => p,
        _ => {
            bot.answer_callback_query(q.id.clone())
                .text("Линк је истекао, пошаљи поново.")
                .await?;
            let _ = bot.edit_message_reply_markup(key.0, key.1).await;
            return Ok(());
        }
    };

    bot.answer_callback_query(q.id.clone()).await?;
    // The question turns into a status line, which also takes the buttons away.
    let status = match mode {
        Mode::Audio => "Преузимам као аудио...",
        Mode::Video => "Преузимам као видео...",
    };
    let _ = bot.edit_message_text(key.0, key.1, status).await;

    let result = match mode {
        Mode::Audio => download_and_send_audio(&bot, job.target, &job.url, &config).await,
        Mode::Video => download_and_send_video(&bot, job.target, &job.url, &config).await,
    };

    if let Err(e) = result {
        error!("Error processing callback for {}: {e}", job.url);
        // Same rule as plain messages: stay quiet in groups.
        if question.chat.is_private() {
            bot.send_message(key.0, user_facing_error(&e.to_string())).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::utils::command::BotCommands;

    /// A message as the Bot API delivers it, in a private chat.
    fn message(fields: serde_json::Value) -> Message {
        let mut json = serde_json::json!({
            "message_id": 7,
            "date": 1_700_000_000,
            "chat": { "id": 42, "type": "private", "first_name": "Ана" },
            "from": { "id": 42, "is_bot": false, "first_name": "Ана" },
        });
        json.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        serde_json::from_value(json).expect("a valid Bot API message")
    }

    #[test]
    fn a_link_after_a_command_is_found() {
        // The command parser keeps nothing after "/audio", so the handler has
        // to look for the link itself.
        assert!(matches!(Cmd::parse("/audio https://youtu.be/dQw4w9WgXcQ", "bot"), Ok(Cmd::Audio)));
        let msg = message(serde_json::json!({
            "text": "/audio https://youtu.be/dQw4w9WgXcQ",
            "entities": [
                { "type": "bot_command", "offset": 0, "length": 6 },
                { "type": "url", "offset": 7, "length": 28 },
            ],
        }));
        assert_eq!(find_link(&msg).as_deref(), Some("https://youtu.be/dQw4w9WgXcQ"));
    }

    #[test]
    fn a_command_can_answer_a_message_with_a_link() {
        let msg = message(serde_json::json!({
            "text": "/audio",
            "entities": [{ "type": "bot_command", "offset": 0, "length": 6 }],
            "reply_to_message": {
                "message_id": 6,
                "date": 1_700_000_000,
                "chat": { "id": 42, "type": "private", "first_name": "Ана" },
                "from": { "id": 43, "is_bot": false, "first_name": "Марко" },
                "text": "ово https://youtu.be/abc",
            },
        }));
        assert_eq!(find_link(&msg), None);
        assert_eq!(
            msg.reply_to_message().and_then(find_link).as_deref(),
            Some("https://youtu.be/abc")
        );
    }

    #[test]
    fn links_hidden_behind_text_are_found() {
        let msg = message(serde_json::json!({
            "text": "погледај ово",
            "entities": [{ "type": "text_link", "offset": 0, "length": 8, "url": "https://youtu.be/abc" }],
        }));
        assert_eq!(find_link(&msg).as_deref(), Some("https://youtu.be/abc"));
    }

    #[test]
    fn anonymous_senders_leave_the_question_open_to_anyone() {
        let ana = message(serde_json::json!({ "text": "x" }));
        assert_eq!(known_sender(&ana), Some(UserId(42)));

        // An admin posting as the group: "from" is GroupAnonymousBot.
        let anonymous: Message = serde_json::from_value(serde_json::json!({
            "message_id": 8,
            "date": 1_700_000_000,
            "chat": { "id": -100500, "type": "supergroup", "title": "Група" },
            "from": { "id": 1087968824, "is_bot": true, "first_name": "Group", "username": "GroupAnonymousBot" },
            "sender_chat": { "id": -100500, "type": "supergroup", "title": "Група" },
            "text": "https://youtu.be/abc",
        }))
        .unwrap();
        assert_eq!(known_sender(&anonymous), None);
    }

    #[test]
    fn unsupported_links_are_told_apart_from_no_link() {
        let msg = message(serde_json::json!({
            "text": "https://example.com/video",
            "entities": [{ "type": "url", "offset": 0, "length": 25 }],
        }));
        assert_eq!(find_link(&msg), None);
        assert!(has_any_link(&msg));

        let msg = message(serde_json::json!({ "text": "ћао" }));
        assert!(!has_any_link(&msg));
    }
}
