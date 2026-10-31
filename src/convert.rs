//! Video files people send the bot, converted so that Telegram plays them.
//!
//! There is no command for it and nothing in /start mentions it: sending a
//! WebM (or MKV, AVI, an MP4 sent as a file, ...) is the request, and an MP4
//! Telegram plays inline comes back. In /audio mode the sound comes back.
//!
//! - In a private chat every video is looked at. One that already plays
//!   inline gets a short note instead of a copy of itself.
//! - In a group only files that Telegram shows as a bare attachment are
//!   converted; ordinary videos are never even downloaded.
//! - api.telegram.org lets bots download files of at most 20 MB.
//! - Uploads come from anyone, so before anything else they are probed with
//!   the same demuxer allowlist the conversion uses (see VIDEO_DEMUXERS).

use std::path::{Path, PathBuf};

use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::ChatAction;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as AsyncCommand;
use tracing::info;

use crate::config::{Config, BOT_API_DOWNLOAD_MB};
use crate::download::{
    deliver_audio, fit_video, named_for_sending, quiet_boost, run_ffmpeg, send_video_file,
    show_chat_action, stream_codec, telegram_friendly_video, titled_path, BoxError,
    VIDEO_DEMUXERS,
};
use crate::types::{Mode, Target};

/// Extensions of video containers, for uploads whose MIME type says nothing
/// useful (`application/octet-stream`).
const VIDEO_EXTENSIONS: &[&str] = &[
    "webm", "mkv", "mp4", "m4v", "mov", "avi", "flv", "wmv", "asf", "mpg", "mpeg", "ts", "m2ts",
    "mts", "3gp", "ogv", "vob", "f4v", "rm", "rmvb", "divx",
];

const CANNOT_OPEN: &str = "Тај фајл не умем да отворим као видео.";
const NO_SOUND: &str = "У том видеу нема звука.";

fn too_big(size: u64, limit: u64) -> String {
    format!(
        "Фајл има {} MB, а Телеграм ботовима не даје фајлове веће од {} MB.",
        size.div_ceil(1024 * 1024),
        limit / (1024 * 1024)
    )
}

/// A video file somebody sent: as a video, or as a file ("document").
#[derive(Debug)]
pub struct Upload {
    file_id: String,
    size: u64,
    /// The name it was sent under, when the client gave one.
    name: Option<String>,
    mime: Option<String>,
    /// Sent as a video, which Telegram already shows with a player.
    sent_as_video: bool,
}

/// How a conversion ended.
pub enum Outcome {
    Sent,
    /// It already plays inline, so there was nothing to send.
    AlreadyPlayable,
}

impl Upload {
    /// The video in a message, if it has one. Stickers (WebM too), GIFs and
    /// round video messages are left alone.
    pub fn from_message(msg: &Message) -> Option<Self> {
        if let Some(video) = msg.video() {
            return Some(Self {
                file_id: video.file.id.clone(),
                size: u64::from(video.file.size),
                name: video.file_name.clone(),
                mime: video.mime_type.as_ref().map(|m| m.essence_str().to_ascii_lowercase()),
                sent_as_video: true,
            });
        }
        let doc = msg.document()?;
        let upload = Self {
            file_id: doc.file.id.clone(),
            size: u64::from(doc.file.size),
            name: doc.file_name.clone(),
            mime: doc.mime_type.as_ref().map(|m| m.essence_str().to_ascii_lowercase()),
            sent_as_video: false,
        };
        let video_mime = upload.mime.as_deref().is_some_and(|m| m.starts_with("video/"));
        let video_name = upload
            .name_extension()
            .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()));
        (video_mime || video_name).then_some(upload)
    }

    /// Whether to act on this upload. In a private chat sending a video is
    /// the request. In a group only files Telegram will not play are worth a
    /// reply: a video message already has a player, whatever its MIME type
    /// says, and an MP4 file plays too.
    pub fn wanted(&self, private: bool) -> bool {
        private || (!self.sent_as_video && !self.plays_inline())
    }

    /// MP4 is what Telegram plays inline (when it holds H.264).
    fn plays_inline(&self) -> bool {
        matches!(self.mime.as_deref(), Some("video/mp4" | "video/x-m4v"))
            || self.name_extension().is_some_and(|e| e == "mp4" || e == "m4v")
    }

    fn name_extension(&self) -> Option<String> {
        let (_, ext) = self.name.as_deref()?.rsplit_once('.')?;
        Some(ext.to_ascii_lowercase())
    }

    /// The extension to save the upload under. Never the sender's own as it
    /// is: a ".m3u8" would make ffmpeg read the file as a playlist.
    fn extension(&self) -> &'static str {
        if let Some(ext) = self
            .name_extension()
            .and_then(|e| VIDEO_EXTENSIONS.iter().find(|v| **v == e).copied())
        {
            return ext;
        }
        match self.mime.as_deref() {
            Some("video/mp4") => "mp4",
            Some("video/webm") => "webm",
            Some("video/x-matroska") => "mkv",
            Some("video/quicktime") => "mov",
            Some("video/x-msvideo" | "video/avi" | "video/msvideo") => "avi",
            _ => "bin",
        }
    }

    /// The name it was sent under, without the extension: what the result is
    /// named, so `clip.webm` comes back as `clip.mp4`.
    fn title(&self) -> String {
        let name = self.name.as_deref().unwrap_or("");
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        stem.trim().to_string()
    }
}

/// Fetch an uploaded video, make it play in Telegram (or take its sound, in
/// /audio mode) and send the result to `target`.
pub async fn convert_upload(
    bot: &Bot,
    target: Target,
    upload: &Upload,
    mode: Mode,
    config: &Config,
) -> Result<Outcome, BoxError> {
    if upload.size > config.max_download_bytes {
        return Err(too_big(upload.size, config.max_download_bytes).into());
    }
    let action = match mode {
        Mode::Video => ChatAction::UploadVideo,
        Mode::Audio => ChatAction::UploadVoice,
    };
    let _status = show_chat_action(bot, target.chat, action);

    let tmp = TempDir::new()?;
    let source = fetch_upload(bot, upload, tmp.path()).await?;
    info!(
        "Converting an uploaded video ({} bytes, {}, mode={mode:?})",
        upload.size,
        upload.mime.as_deref().unwrap_or("no MIME type")
    );
    let title = upload.title();
    match prepare(&source, tmp.path(), mode, upload.sent_as_video, &title, config).await? {
        Prepared::AlreadyPlayable => Ok(Outcome::AlreadyPlayable),
        Prepared::Video(path) => {
            send_video_file(bot, &target, &path, config).await?;
            info!("Sent the converted video");
            Ok(Outcome::Sent)
        }
        Prepared::Audio { path, boost_db } => {
            deliver_audio(bot, &target, &path, &title, tmp.path(), boost_db, config).await?;
            info!("Sent the sound of an uploaded video");
            Ok(Outcome::Sent)
        }
    }
}

/// Get the uploaded file from Telegram into `dir`.
async fn fetch_upload(bot: &Bot, upload: &Upload, dir: &Path) -> Result<PathBuf, BoxError> {
    let file = bot.get_file(upload.file_id.clone()).await.map_err(|e| -> BoxError {
        // A Bot API server of your own without --local keeps the 20 MB cap.
        if e.to_string().contains("file is too big") {
            too_big(upload.size, BOT_API_DOWNLOAD_MB * 1024 * 1024).into()
        } else {
            e.into()
        }
    })?;
    let dest = dir.join(format!("upload.{}", upload.extension()));
    // A self-hosted Bot API server in --local mode answers with a path on its
    // own disk instead of one to download.
    let local = Path::new(&file.path);
    if local.is_absolute() && local.is_file() {
        tokio::fs::copy(local, &dest).await?;
    } else {
        let mut out = tokio::fs::File::create(&dest).await?;
        bot.download_file(&file.path, &mut out).await?;
        out.flush().await?;
    }
    Ok(dest)
}

/// What an upload turns into.
#[derive(Debug)]
enum Prepared {
    Video(PathBuf),
    Audio { path: PathBuf, boost_db: Option<f64> },
    /// A video Telegram plays already, sent as one.
    AlreadyPlayable,
}

/// Turn the downloaded upload into what gets sent. Kept apart from Telegram
/// so it can be tested on its own.
async fn prepare(
    source: &Path,
    dir: &Path,
    mode: Mode,
    sent_as_video: bool,
    title: &str,
    config: &Config,
) -> Result<Prepared, BoxError> {
    let probe = probe_streams(&config.ffprobe_bin, source).await.ok_or(CANNOT_OPEN)?;

    // Sound only (an Opus WebM, say), or /audio mode: the sound comes back.
    if !probe.has_video || matches!(mode, Mode::Audio) {
        if !probe.has_audio {
            return Err(if probe.has_video { NO_SOUND } else { CANNOT_OPEN }.into());
        }
        let path = extract_audio(source, dir, title, config).await?;
        let boost_db = quiet_boost(&path, config).await;
        return Ok(Prepared::Audio { path, boost_db });
    }

    let ready = telegram_friendly_video(source, dir, config).await?;
    if ready == source && sent_as_video {
        return Ok(Prepared::AlreadyPlayable);
    }
    let fitted = fit_video(&ready, dir, config).await?;
    // `clip.webm` comes back as `clip.mp4`.
    Ok(Prepared::Video(named_for_sending(&fitted, dir, title)))
}

/// The kinds of stream in a file.
struct Streams {
    has_video: bool,
    has_audio: bool,
}

/// What is in an uploaded file, or `None` when it is not a container the
/// conversion is allowed to open. Cover art does not count as video: a song
/// with its artwork comes back as a song, not as a still picture.
async fn probe_streams(ffprobe: &Path, path: &Path) -> Option<Streams> {
    let out = AsyncCommand::new(ffprobe)
        .args(["-v", "error", "-format_whitelist", VIDEO_DEMUXERS])
        .args(["-show_entries", "stream=codec_type:stream_disposition=attached_pic"])
        .args(["-of", "compact=p=0:nk=1"])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // One line per stream: "video|0", "audio|0", or "video|1" for cover art.
    let streams = String::from_utf8_lossy(&out.stdout);
    let streams: Vec<&str> = streams.lines().map(str::trim).collect();
    Some(Streams {
        has_video: streams.contains(&"video|0"),
        has_audio: streams.iter().any(|s| s.starts_with("audio")),
    })
}

/// The sound of an uploaded file on its own, copied rather than re-encoded:
/// sendAudio refuses a video. Anything that is not MP3 or AAC gets converted
/// to M4A when it is sent.
async fn extract_audio(source: &Path, dir: &Path, title: &str, config: &Config) -> Result<PathBuf, BoxError> {
    let codec = stream_codec(&config.ffprobe_bin, source, "a:0").await.unwrap_or_default();
    let ext = match codec.as_str() {
        "aac" => "m4a",
        "mp3" => "mp3",
        "opus" => "opus",
        "vorbis" => "ogg",
        "flac" => "flac",
        // Matroska takes any codec.
        _ => "mka",
    };
    let sound_dir = dir.join("sound");
    std::fs::create_dir_all(&sound_dir)?;
    let dest = titled_path(&sound_dir, title, "audio", ext);
    run_ffmpeg(
        &[
            "-format_whitelist".as_ref(), VIDEO_DEMUXERS.as_ref(),
            "-i".as_ref(), source.as_ref(),
            "-map".as_ref(), "0:a:0".as_ref(),
            "-c:a".as_ref(), "copy".as_ref(),
            dest.as_ref(),
        ],
        config,
    )
    .await?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message in a private chat, with the given media fields.
    fn message(media: serde_json::Value) -> Message {
        let mut json = serde_json::json!({
            "message_id": 7,
            "date": 1_700_000_000,
            "chat": { "id": 42, "type": "private", "first_name": "Ана" },
            "from": { "id": 42, "is_bot": false, "first_name": "Ана" },
        });
        json.as_object_mut().unwrap().extend(media.as_object().unwrap().clone());
        serde_json::from_value(json).expect("a valid Bot API message")
    }

    fn file(id: &str) -> serde_json::Value {
        serde_json::json!({ "file_id": id, "file_unique_id": id, "file_size": 1_234_567 })
    }

    fn document(name: &str, mime: &str) -> Message {
        let mut doc = file("doc");
        doc["file_name"] = name.into();
        doc["mime_type"] = mime.into();
        message(serde_json::json!({ "document": doc }))
    }

    #[test]
    fn webm_files_are_converted_everywhere() {
        let upload = Upload::from_message(&document("clip.webm", "video/webm")).unwrap();
        assert!(upload.wanted(false) && upload.wanted(true));
        assert!(!upload.sent_as_video);
        assert_eq!(upload.extension(), "webm");
        assert_eq!(upload.title(), "clip");

        let mkv = Upload::from_message(&document("Some Film.MKV", "application/octet-stream")).unwrap();
        assert!(mkv.wanted(false));
        assert_eq!(mkv.extension(), "mkv");
    }

    #[test]
    fn mp4s_are_only_looked_at_in_private_chats() {
        let doc = Upload::from_message(&document("clip.mp4", "video/mp4")).unwrap();
        assert!(!doc.wanted(false));
        assert!(doc.wanted(true));

        let mut video = file("vid");
        video["width"] = 1280.into();
        video["height"] = 720.into();
        video["duration"] = 12.into();
        video["mime_type"] = "video/mp4".into();
        let upload = Upload::from_message(&message(serde_json::json!({ "video": video.clone() }))).unwrap();
        assert!(upload.sent_as_video);
        assert!(!upload.wanted(false));
        assert!(upload.wanted(true));
        // Phones often send no file name.
        assert_eq!(upload.title(), "");
        assert_eq!(upload.extension(), "mp4");

        // A video message already has a player in a group, whatever its MIME
        // type says, so it is not posted again.
        video["mime_type"] = "video/quicktime".into();
        let upload = Upload::from_message(&message(serde_json::json!({ "video": video }))).unwrap();
        assert!(!upload.wanted(false));
    }

    #[test]
    fn other_files_stickers_and_gifs_are_left_alone() {
        assert!(Upload::from_message(&document("notes.pdf", "application/pdf")).is_none());

        let mut sticker = file("st");
        sticker["width"] = 512.into();
        sticker["height"] = 512.into();
        sticker["type"] = "regular".into();
        sticker["is_animated"] = false.into();
        sticker["is_video"] = true.into();
        assert!(Upload::from_message(&message(serde_json::json!({ "sticker": sticker }))).is_none());

        // The Bot API sends a GIF as both "animation" and "document".
        let mut gif = file("gif");
        gif["width"] = 320.into();
        gif["height"] = 240.into();
        gif["duration"] = 3.into();
        gif["file_name"] = "funny.mp4".into();
        gif["mime_type"] = "video/mp4".into();
        let msg = message(serde_json::json!({ "animation": gif, "document": gif }));
        assert!(Upload::from_message(&msg).is_none());
    }

    #[test]
    fn the_saved_copy_never_takes_a_dangerous_extension() {
        assert_eq!(Upload::from_message(&document("x.m3u8", "video/webm")).unwrap().extension(), "webm");
        assert_eq!(Upload::from_message(&document("x.m3u8", "video/x-unknown")).unwrap().extension(), "bin");
    }

    // --- With ffmpeg ---------------------------------------------------------

    fn make(dir: &Path, name: &str, size: &str, codecs: &[&str]) -> PathBuf {
        let out = dir.join(name);
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={size}:rate=25:duration=1"))
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(codecs)
            .arg("-shortest")
            .arg(&out)
            .status()
            .expect("ffmpeg must be installed for this test");
        assert!(status.success(), "could not make {name}");
        out
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn a_webm_comes_back_as_an_mp4_named_after_it() {
        let dir = TempDir::new().unwrap();
        let src = make(dir.path(), "upload.webm", "853x480", &["-c:v", "libvpx-vp9", "-c:a", "libopus"]);
        let config = Config::from_env();
        match prepare(&src, dir.path(), Mode::Video, false, "funny clip", &config).await.unwrap() {
            Prepared::Video(path) => {
                assert_eq!(path.file_name().unwrap(), "funny clip.mp4");
                assert_eq!(stream_codec(&config.ffprobe_bin, &path, "v:0").await.as_deref(), Some("h264"));
                assert_eq!(stream_codec(&config.ffprobe_bin, &path, "a:0").await.as_deref(), Some("aac"));
            }
            other => panic!("expected a video, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn a_playable_video_is_left_alone_but_a_playable_file_is_sent_as_video() {
        let dir = TempDir::new().unwrap();
        let src = make(
            dir.path(), "upload.mp4", "640x360",
            &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"],
        );
        let config = Config::from_env();
        let as_video = prepare(&src, dir.path(), Mode::Video, true, "", &config).await.unwrap();
        assert!(matches!(as_video, Prepared::AlreadyPlayable));
        let as_file = prepare(&src, dir.path(), Mode::Video, false, "", &config).await.unwrap();
        assert!(matches!(as_file, Prepared::Video(ref p) if p.file_name().unwrap() == "video.mp4"));
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn audio_mode_sends_the_sound_alone() {
        let dir = TempDir::new().unwrap();
        let src = make(
            dir.path(), "upload.mp4", "640x360",
            &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"],
        );
        let config = Config::from_env();
        match prepare(&src, dir.path(), Mode::Audio, true, "song", &config).await.unwrap() {
            Prepared::Audio { path, .. } => {
                assert_eq!(path.file_name().unwrap(), "song.m4a");
                assert_eq!(stream_codec(&config.ffprobe_bin, &path, "v:0").await, None);
                assert_eq!(stream_codec(&config.ffprobe_bin, &path, "a:0").await.as_deref(), Some("aac"));
            }
            other => panic!("expected audio, got {other:?}"),
        }

        let silent = dir.path().join("silent.webm");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=320x240:duration=1"])
            .args(["-c:v", "libvpx-vp9"])
            .arg(&silent)
            .status()
            .unwrap();
        assert!(status.success());
        let err = prepare(&silent, dir.path(), Mode::Audio, false, "", &config).await.unwrap_err();
        assert_eq!(err.to_string(), NO_SOUND);
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn a_webm_with_only_sound_comes_back_as_audio() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("upload.webm");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "sine=duration=1", "-c:a", "libopus"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());
        let config = Config::from_env();
        let prepared = prepare(&src, dir.path(), Mode::Video, false, "tune", &config).await.unwrap();
        assert!(matches!(prepared, Prepared::Audio { ref path, .. } if path.file_name().unwrap() == "tune.opus"));
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn a_song_with_cover_art_comes_back_as_a_song() {
        let dir = TempDir::new().unwrap();
        let song = dir.path().join("song.m4a");
        let cover = dir.path().join("cover.jpg");
        let src = dir.path().join("upload.mp4");
        for args in [
            vec!["-f", "lavfi", "-i", "sine=duration=1", "-c:a", "aac", song.to_str().unwrap()],
            vec!["-f", "lavfi", "-i", "testsrc=size=300x300", "-frames:v", "1", cover.to_str().unwrap()],
            vec![
                "-i", song.to_str().unwrap(), "-i", cover.to_str().unwrap(), "-map", "0", "-map", "1",
                "-c", "copy", "-disposition:v:0", "attached_pic", src.to_str().unwrap(),
            ],
        ] {
            let status = std::process::Command::new("ffmpeg")
                .args(["-y", "-loglevel", "error"])
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let config = Config::from_env();
        let prepared = prepare(&src, dir.path(), Mode::Video, false, "song", &config).await.unwrap();
        assert!(matches!(prepared, Prepared::Audio { ref path, .. } if path.file_name().unwrap() == "song.m4a"));
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn a_playlist_posing_as_a_video_is_not_opened() {
        let dir = TempDir::new().unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "do not send me").unwrap();
        let fake = dir.path().join("upload.webm");
        std::fs::write(&fake, format!("ffconcat version 1.0\nfile '{}'\n", secret.display())).unwrap();
        let config = Config::from_env();
        let err = prepare(&fake, dir.path(), Mode::Video, false, "", &config).await.unwrap_err();
        assert_eq!(err.to_string(), CANNOT_OPEN);
    }
}
