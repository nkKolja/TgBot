use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use teloxide::prelude::*;
use teloxide::types::{ChatAction, InputFile, ReplyParameters};
use teloxide::RequestError;
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;
use tracing::{info, warn};

use crate::config::{tool_path, Config};
use crate::gallery::{send_gallery, GalleryFailure};
use crate::helpers::{
    detect_browser, find_prefixed_file, is_auth_failure, is_instagram_post, is_live_skip,
    is_no_video, media_duration, video_dimensions, ytdlp_auth_args, YTDLP_ONE_VIDEO,
};
use crate::types::Target;

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Leave room for the multipart envelope so a file that measures just under the
/// cap does not get rejected by the API.
pub(crate) fn upload_budget(config: &Config) -> u64 {
    config.max_upload_bytes.saturating_sub(512 * 1024)
}

pub(crate) fn file_size(path: &Path) -> u64 {
    path.metadata().map(|m| m.len()).unwrap_or(0)
}

fn human_mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

pub(crate) async fn run_ffmpeg(args: &[&std::ffi::OsStr], config: &Config) -> Result<(), BoxError> {
    // One encode at a time by default: on an old CPU two parallel x264 runs
    // each take twice as long and both blow the timeout.
    let _slot = config.encode_slots.acquire().await?;

    let mut cmd = AsyncCommand::new(&config.ffmpeg_bin);
    cmd.args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn()?;
    let out = match tokio::time::timeout(config.encode_timeout, child.wait_with_output()).await {
        Ok(out) => out?,
        Err(_) => {
            return Err(format!(
                "ffmpeg cancelled: encode exceeded {}s limit",
                config.encode_timeout.as_secs()
            )
            .into())
        }
    };
    if !out.status.success() {
        return Err(format!("ffmpeg failed: {}", tail(&String::from_utf8_lossy(&out.stderr), 600)).into());
    }
    Ok(())
}

/// The time allowed for fetching one link (DOWNLOAD_TIMEOUT_SECS), shared by
/// every download attempt for it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    at: Instant,
    total: Duration,
}

impl Deadline {
    pub(crate) fn start(total: Duration) -> Self {
        Self { at: Instant::now() + total, total }
    }

    /// Time left, or a timeout error once (almost) none is left.
    pub(crate) fn left(&self) -> Result<Duration, BoxError> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left < Duration::from_secs(2) {
            return Err(self.expired("download"));
        }
        Ok(left)
    }

    fn expired(&self, what: &str) -> BoxError {
        format!("{what} cancelled: download exceeded the {}s limit", self.total.as_secs()).into()
    }
}

pub(crate) fn is_timeout(e: &BoxError) -> bool {
    e.to_string().contains("cancelled")
}

/// One field (`codec_name`, `pix_fmt`) of the first stream of the given kind
/// (`v:0`, `a:0`).
async fn stream_field(ffprobe: &Path, path: &Path, selector: &str, field: &str) -> Option<String> {
    // Not CSV: for a stream with side data (a phone video's rotation) that
    // prints "h264," and the codec no longer compares equal.
    let out = AsyncCommand::new(ffprobe)
        .args(["-v", "error", "-select_streams", selector])
        .args(["-show_entries", &format!("stream={field}"), "-of", "default=nw=1:nk=1"])
        .arg(path)
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value = stdout.lines().next().unwrap_or("").trim().to_ascii_lowercase();
    (out.status.success() && !value.is_empty()).then_some(value)
}

/// Codec name of the first stream of the given kind (`v:0`, `a:0`).
pub(crate) async fn stream_codec(ffprobe: &Path, path: &Path, selector: &str) -> Option<String> {
    stream_field(ffprobe, path, selector, "codec_name").await
}

/// Codec and pixel format of the video, in one probe. "V" passes over cover
/// art, which some files carry as a still-picture video stream.
async fn video_format(ffprobe: &Path, path: &Path) -> (Option<String>, Option<String>) {
    let Ok(out) = AsyncCommand::new(ffprobe)
        .args(["-v", "error", "-select_streams", "V:0"])
        .args(["-show_entries", "stream=codec_name,pix_fmt", "-of", "default=nw=1"])
        .arg(path)
        .output()
        .await
    else {
        return (None, None);
    };
    let (mut codec, mut pix_fmt) = (None, None);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        match line.split_once('=') {
            Some(("codec_name", v)) => codec = Some(v.trim().to_ascii_lowercase()),
            Some(("pix_fmt", v)) => pix_fmt = Some(v.trim().to_ascii_lowercase()),
            _ => {}
        }
    }
    let known = |v: Option<String>| v.filter(|v| !v.is_empty() && v != "unknown");
    (known(codec), known(pix_fmt))
}

/// Demuxers ffmpeg may use on a video it converts: real containers only, none
/// of the playlist formats (HLS, concat) that open further files. Passed as
/// `-format_whitelist`, it keeps a crafted upload from making ffmpeg read
/// files off this machine into a video that then gets sent back.
const VIDEO_DEMUXERS: &str = "mov,matroska,avi,flv,mpegts,mpeg,asf,ogg,rm,ivf,gif";

/// Make a video Telegram will play inline: H.264 + AAC/MP3 in MP4. Each part
/// that is already right is copied, so most downloads cost a quick remux or
/// nothing at all; only foreign codecs (VP9, AV1, Opus) are re-encoded.
pub(crate) async fn telegram_friendly_video(path: &Path, dir: &Path, config: &Config) -> Result<PathBuf, BoxError> {
    let (vcodec, pix_fmt) = video_format(&config.ffprobe_bin, path).await;
    let acodec = stream_codec(&config.ffprobe_bin, path, "a:0").await;
    // Phones only decode 8-bit 4:2:0 H.264 in hardware; High 10 or 4:4:4
    // (screen recorders, some anime encodes) shows up as a black or broken video.
    let video_ok = vcodec.as_deref() == Some("h264")
        && matches!(pix_fmt.as_deref(), Some("yuv420p") | Some("yuvj420p"));
    let audio_ok = matches!(acodec.as_deref(), None | Some("aac") | Some("mp3"));
    let is_mp4 = matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("mp4") | Some("m4v")
    );
    if video_ok && audio_ok && is_mp4 {
        return Ok(path.to_path_buf());
    }
    if vcodec.is_none() {
        // Could not probe it; let send_video try and fall back to a document.
        warn!("Could not read the video codec, sending as downloaded");
        return Ok(path.to_path_buf());
    }

    info!(
        "Converting for Telegram (video {} -> {}, audio {} -> {})",
        vcodec.as_deref().unwrap_or("?"),
        if video_ok { "copy" } else { "h264" },
        acodec.as_deref().unwrap_or("none"),
        if audio_ok { "copy" } else { "aac" },
    );
    let dest = dir.join("telegram.mp4");
    let mut args: Vec<std::ffi::OsString> = vec![
        "-format_whitelist".into(), VIDEO_DEMUXERS.into(),
        "-i".into(), path.into(),
        "-map".into(), "0:V:0".into(),
        "-map".into(), "0:a:0?".into(),
    ];
    if video_ok {
        args.extend(["-c:v".into(), "copy".into()]);
    } else {
        args.extend([
            "-c:v".into(), "libx264".into(),
            "-preset".into(), config.x264_preset.as_str().into(),
            "-crf".into(), "24".into(),
            "-pix_fmt".into(), "yuv420p".into(),
            // Never upscale; cap the width at 1280. x264 needs even sides,
            // and odd ones (853x480) are common in WebMs.
            "-vf".into(), "scale=w='trunc(min(1280,iw)/2)*2':h=-2".into(),
        ]);
    }
    if audio_ok {
        args.extend(["-c:a".into(), "copy".into()]);
    } else {
        args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "128k".into()]);
    }
    args.extend(["-movflags".into(), "+faststart".into(), dest.as_os_str().into()]);
    let argv: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
    run_ffmpeg(&argv, config).await.map_err(|e| format!("re-encode failed: {e}"))?;

    if file_size(&dest) == 0 {
        return Err("Re-encoded file is empty".into());
    }
    Ok(dest)
}

// --- Sending -----------------------------------------------------------------

/// Keeps "sending video…" showing in the chat while the bot works, which can
/// take a minute. Telegram clears an action after five seconds, so it is sent
/// again until the guard is dropped.
struct ChatActionGuard(tokio::task::JoinHandle<()>);

impl Drop for ChatActionGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn show_chat_action(bot: &Bot, chat: ChatId, action: ChatAction) -> ChatActionGuard {
    let bot = bot.clone();
    ChatActionGuard(tokio::spawn(async move {
        loop {
            // Purely cosmetic: a failure here is not worth reporting.
            let _ = bot.send_chat_action(chat, action).await;
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    }))
}

/// Run a Telegram request, waiting out "429 Too Many Requests" instead of
/// failing. Telegram allows roughly one message a second per chat and 20 a
/// minute in groups, and an album counts once per item, so carousels and
/// multi-part audio are what run into it.
pub(crate) async fn with_retry<T, F, Fut>(mut send: F) -> Result<T, RequestError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RequestError>>,
{
    const MAX_RETRIES: u32 = 3;
    let mut retries = 0;
    loop {
        match send().await {
            Err(RequestError::RetryAfter(wait)) if retries < MAX_RETRIES => {
                retries += 1;
                let wait = wait.duration() + Duration::from_millis(500);
                warn!("Telegram flood control, retrying in {}s", wait.as_secs());
                tokio::time::sleep(wait).await;
            }
            result => return result,
        }
    }
}

/// Whether Telegram looked at the upload and turned it down (wrong type, a
/// codec it cannot play), so that sending the file another way may work. A
/// network error is not that: the upload may well have arrived, and sending it
/// again as a document would post it twice.
pub(crate) fn refused(e: &RequestError) -> bool {
    matches!(e, RequestError::Api(_))
}

/// Send media with `req`. If Telegram refuses it as that kind of media (an
/// odd codec or container), send the same file as a plain document, which
/// still gets it there.
pub(crate) async fn send_or_document<R>(
    bot: &Bot,
    target: &Target,
    path: &Path,
    caption: Option<&str>,
    req: R,
) -> Result<(), BoxError>
where
    R: Request<Err = RequestError>,
{
    match with_retry(|| req.send_ref()).await {
        Ok(_) => Ok(()),
        Err(e) if refused(&e) => {
            warn!("Telegram refused {} ({e}), sending it as a document", path.display());
            send_document_file(bot, target, path, caption).await
        }
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn send_document_file(
    bot: &Bot,
    target: &Target,
    path: &Path,
    caption: Option<&str>,
) -> Result<(), BoxError> {
    let mut req = bot.send_document(target.chat, InputFile::file(path.to_path_buf()));
    if let Some(caption) = caption.filter(|c| !c.is_empty()) {
        req = req.caption(caption);
    }
    if let Some(id) = target.reply_to {
        req = req.reply_parameters(ReplyParameters::new(id));
    }
    with_retry(|| req.send_ref()).await?;
    Ok(())
}

pub(crate) async fn send_video_file(
    bot: &Bot,
    target: &Target,
    path: &Path,
    config: &Config,
) -> Result<(), BoxError> {
    let (w, h) = video_dimensions(&config.ffprobe_bin, path).await;

    let mut req = bot
        .send_video(target.chat, InputFile::file(path.to_path_buf()))
        .supports_streaming(true);
    if let Some(id) = target.reply_to {
        req = req.reply_parameters(ReplyParameters::new(id));
    }
    if let Some(w) = w {
        req = req.width(w);
    }
    if let Some(h) = h {
        req = req.height(h);
    }
    // Like the size, passed rather than left to Telegram's own probing.
    if let Some(secs) = media_duration(&config.ffprobe_bin, path).await.filter(|s| *s > 0.0) {
        req = req.duration(secs.round() as u32);
    }
    send_or_document(bot, target, path, None, req).await
}

async fn send_audio_file(
    bot: &Bot,
    target: &Target,
    path: &Path,
    title: &str,
    config: &Config,
) -> Result<(), BoxError> {
    let mut req = bot.send_audio(target.chat, InputFile::file(path.to_path_buf()));
    if !title.is_empty() {
        req = req.title(title);
    }
    if let Some(secs) = media_duration(&config.ffprobe_bin, path).await {
        req = req.duration(secs as u32);
    }
    if let Some(id) = target.reply_to {
        req = req.reply_parameters(ReplyParameters::new(id));
    }
    // sendAudio only accepts MP3/M4A.
    send_or_document(bot, target, path, Some(title), req).await
}

// --- Shrinking ---------------------------------------------------------------

/// Build an output path in `dir` named after the title, so desktop Telegram
/// shows something meaningful instead of `shrunk.m4a`.
fn titled_path(dir: &Path, title: &str, fallback: &str, ext: &str) -> PathBuf {
    let safe: String = title
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == ':' { '_' } else { c })
        .collect();
    let stem = if safe.trim().is_empty() { fallback } else { safe.trim() };
    // Filesystems cap names at 255 bytes, and Cyrillic takes two bytes a
    // character, emoji (all over TikTok titles) four.
    let mut bytes = 0;
    let stem: String = stem
        .chars()
        .take_while(|c| {
            bytes += c.len_utf8();
            bytes <= 200
        })
        .collect();
    dir.join(format!("{stem}.{ext}"))
}

/// Move a finished video to a file named after its title, or after the file
/// it was made from: that name is what people see when they save it, rather
/// than `telegram.mp4`. Keeps the old name if the move fails.
pub(crate) fn named_for_sending(path: &Path, dir: &Path, title: &str) -> PathBuf {
    let out_dir = dir.join("out");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let dest = titled_path(&out_dir, title, "video", ext);
    match std::fs::create_dir_all(&out_dir).and_then(|()| std::fs::rename(path, &dest)) {
        Ok(()) => dest,
        Err(e) => {
            warn!("Could not rename {} for sending: {e}", path.display());
            path.to_path_buf()
        }
    }
}

/// Audio quieter than this gets the loudness fallback. Normal streaming audio
/// sits around -14 to -18 LUFS; badly mastered uploads can be -29 or lower.
const QUIET_LUFS: f64 = -24.0;
/// Level the fallback boosts quiet audio to.
const TARGET_LUFS: f64 = -16.0;

/// YouTube's "stable volume" (DRC) audio streams, which yt-dlp names
/// `<itag>-drc`. They are compressed and much louder than the original track.
fn drc_audio_format(budget_mb: u64) -> String {
    let mut alts = Vec::new();
    for base in ["ba[format_id$=-drc][language_preference>=10]", "ba[format_id$=-drc]"] {
        for size in ["filesize", "filesize_approx"] {
            alts.push(format!("{base}[ext=m4a][{size}<{budget_mb}M]"));
        }
    }
    alts.join("/")
}

fn is_youtube(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?']).next().unwrap_or("");
    let host = host.strip_prefix("www.").or_else(|| host.strip_prefix("m.")).unwrap_or(host);
    matches!(host, "youtube.com" | "youtu.be" | "music.youtube.com")
}

/// macOS ffmpeg builds usually include Apple's AudioToolbox AAC encoder, which
/// is several times faster than ffmpeg's own on an old CPU. Checked once.
fn aac_encoder(config: &Config) -> &'static str {
    static ENCODER: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    ENCODER.get_or_init(|| {
        let has_at = std::process::Command::new(&config.ffmpeg_bin)
            .args(["-hide_banner", "-encoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(" aac_at "))
            .unwrap_or(false);
        let enc = if has_at { "aac_at" } else { "aac" };
        info!("Using AAC encoder: {enc}");
        enc
    })
}

/// Integrated loudness (EBU R128) of up to 10 minutes of the file, skipping the
/// first minute of longer files (intros are often music or silence). Decode
/// only, so it takes about a second.
async fn sample_loudness(src: &Path, config: &Config) -> Option<f64> {
    let duration = media_duration(&config.ffprobe_bin, src).await.unwrap_or(0.0);
    let mut cmd = AsyncCommand::new(&config.ffmpeg_bin);
    cmd.args(["-nostdin", "-hide_banner", "-nostats"]);
    if duration > 180.0 {
        cmd.args(["-ss", "60"]);
    }
    cmd.args(["-t", "600", "-i"])
        .arg(src)
        .args(["-vn", "-af", "ebur128", "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.spawn().ok()?.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // The summary comes last; its first "I:" line is the integrated loudness.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let summary = &stderr[stderr.rfind("Summary:")?..];
    let lufs: f64 = summary
        .lines()
        .find_map(|l| l.trim().strip_prefix("I:"))?
        .trim()
        .trim_end_matches("LUFS")
        .trim()
        .parse()
        .ok()?;
    // -70 is what ebur128 reports for silence.
    (lufs > -60.0).then_some(lufs)
}

/// Gain plus a peak limiter at about -1.5 dBFS so the boost does not clip.
fn boost_filter(gain_db: f64) -> String {
    format!("volume={gain_db:.2}dB,alimiter=limit=0.84:level=false")
}

/// Re-encode audio at whatever bitrate fits the upload budget. Returns `None`
/// when even the floor bitrate would not fit, in which case the caller splits.
async fn shrink_audio(
    src: &Path,
    dir: &Path,
    title: &str,
    duration: f64,
    filter: &str,
    config: &Config,
) -> Result<Option<PathBuf>, BoxError> {
    const FLOOR_KBPS: u64 = 32;

    if duration <= 0.0 {
        return Ok(None);
    }
    // bits available / seconds / 1000 = kbit/s, minus a little for the container.
    let target_kbps = ((upload_budget(config) * 8) as f64 / duration / 1000.0 * 0.95) as u64;
    let bitrate_kbps = target_kbps.clamp(FLOOR_KBPS, 128);
    if target_kbps < FLOOR_KBPS {
        return Ok(None);
    }

    let dest = titled_path(dir, title, "audio", "m4a");
    info!("Re-encoding audio to {bitrate_kbps}k to fit the upload limit");
    let bitrate = format!("{bitrate_kbps}k");
    // Mono at very low bitrates: stereo at 48k and below sounds noticeably worse.
    let channels = if bitrate_kbps <= 48 { "1" } else { "2" };
    run_ffmpeg(
        &[
            "-i".as_ref(), src.as_ref(),
            "-vn".as_ref(),
            "-af".as_ref(), filter.as_ref(),
            "-c:a".as_ref(), aac_encoder(config).as_ref(),
            "-b:a".as_ref(), bitrate.as_ref(),
            "-ac".as_ref(), channels.as_ref(),
            "-movflags".as_ref(), "+faststart".as_ref(),
            dest.as_ref(),
        ],
        config,
    )
    .await?;

    if file_size(&dest) > upload_budget(config) {
        return Ok(None);
    }
    Ok(Some(dest))
}

/// Re-encode to M4A when the container is something Telegram will not play as
/// audio (opus/webm, for instance), or when `boost` asks for a level change.
/// Returns the original path if it is already fine or if the encode fails.
async fn normalize_audio(
    src: &Path,
    dir: &Path,
    title: &str,
    boost: Option<(&str, u64)>,
    config: &Config,
) -> PathBuf {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // Sites with no audio-only format (TikTok, clip sites) leave yt-dlp's
    // audio selector to fall back on the whole video, which sendAudio would
    // get as is. "V" skips cover art, which audio files may carry.
    let has_video = stream_codec(&config.ffprobe_bin, src, "V:0").await.is_some();
    if boost.is_none() && !has_video && matches!(ext.as_str(), "mp3" | "m4a" | "mp4" | "aac") {
        return src.to_path_buf();
    }
    let (filter, kbps) = boost.unwrap_or(("anull", 128));
    let bitrate = format!("{kbps}k");

    let dest = titled_path(dir, title, "audio", "m4a");
    info!("Encoding .{ext} audio to {kbps}k m4a for Telegram (filter: {filter})");
    match run_ffmpeg(
        &[
            "-i".as_ref(), src.as_ref(),
            "-vn".as_ref(),
            "-af".as_ref(), filter.as_ref(),
            "-c:a".as_ref(), aac_encoder(config).as_ref(),
            "-b:a".as_ref(), bitrate.as_ref(),
            "-movflags".as_ref(), "+faststart".as_ref(),
            dest.as_ref(),
        ],
        config,
    )
    .await
    {
        Ok(()) if file_size(&dest) > 0 => dest,
        Ok(()) => src.to_path_buf(),
        Err(e) => {
            warn!("Audio conversion failed ({e}), sending as-is");
            src.to_path_buf()
        }
    }
}

/// Cut a file into equal chunks that each fit the upload budget.
async fn split_media(
    src: &Path,
    dir: &Path,
    duration: f64,
    config: &Config,
) -> Result<Vec<PathBuf>, BoxError> {
    if duration <= 0.0 {
        return Err("Cannot split a file of unknown duration".into());
    }
    let budget = upload_budget(config);
    let size = file_size(src);
    let parts = (size as f64 / budget as f64).ceil().max(2.0) as u64;
    // A little extra slack so variable bitrate does not push one chunk over.
    let segment_secs = ((duration / parts as f64) * 0.95).max(1.0) as u64;
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("m4a");

    info!("Splitting into ~{parts} parts of {segment_secs}s each");
    let parts_dir = dir.join("parts");
    std::fs::create_dir_all(&parts_dir)?;
    let pattern = parts_dir.join(format!("part_%03d.{ext}"));
    run_ffmpeg(
        &[
            "-i".as_ref(), src.as_ref(),
            "-c".as_ref(), "copy".as_ref(),
            "-map".as_ref(), "0".as_ref(),
            "-f".as_ref(), "segment".as_ref(),
            "-segment_time".as_ref(), segment_secs.to_string().as_ref(),
            "-reset_timestamps".as_ref(), "1".as_ref(),
            pattern.as_ref(),
        ],
        config,
    )
    .await?;

    let mut chunks: Vec<PathBuf> = std::fs::read_dir(&parts_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    chunks.sort();
    if chunks.is_empty() {
        return Err("Splitting produced no files".into());
    }
    Ok(chunks)
}

/// Last `max_chars` characters, cut at a line boundary. yt-dlp puts the fatal
/// ERROR line last, so the tail is the part worth reporting.
pub(crate) fn tail(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().skip(count - max_chars).collect();
    match cut.find('\n') {
        Some(i) => cut[i + 1..].to_string(),
        None => cut,
    }
}

// --- External downloaders ----------------------------------------------------

/// What a yt-dlp or gallery-dl run produced. Only a failure to start and a
/// timeout are errors here; a non-zero exit shows up in `success`, so the
/// caller can still look at whatever was downloaded.
pub(crate) struct ToolOutput {
    pub success: bool,
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl ToolOutput {
    /// The end of whichever stream carries the error, which is where the fatal
    /// message is.
    pub(crate) fn failure_detail(&self) -> String {
        let detail = if self.stderr.trim().is_empty() { &self.stdout } else { &self.stderr };
        tail(detail, 1200)
    }
}

pub(crate) async fn run_tool(
    name: &str,
    bin: &Path,
    args: &[&str],
    extra: &[String],
    url: &str,
    deadline: Deadline,
) -> Result<ToolOutput, BoxError> {
    let timeout = deadline.left()?;
    let mut cmd = AsyncCommand::new(bin);
    cmd.args(args).args(extra).arg(url).kill_on_drop(true);
    if let Some(path) = tool_path() {
        cmd.env("PATH", path);
    }
    // Without explicit pipes the child inherits our stdio and wait_with_output()
    // hands back empty buffers — which silently blinds every check we make on
    // the tool's output.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| format!("{name} could not be started ({}): {e}", bin.display()))?;
    let pid = child.id();
    let output = child.wait_with_output();
    tokio::pin!(output);
    tokio::select! {
        result = &mut output => match result {
            Ok(out) => Ok(ToolOutput {
                success: out.status.success(),
                status: out.status,
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            Err(e) => Err(format!("{name} io error: {e}").into()),
        },
        () = tokio::time::sleep(timeout) => {
            // kill_on_drop only reaches the tool itself, once `output` is
            // dropped. What the tool started (yt-dlp hands merges and live
            // streams to ffmpeg) would run on, so it goes first, while it can
            // still be found as the tool's child.
            if let Some(pid) = pid {
                kill_children(pid).await;
            }
            Err(deadline.expired(name))
        }
    }
}

/// Kill the processes `pid` started. They can only be told apart while `pid`
/// is alive: after that they are adopted by launchd (or init).
async fn kill_children(pid: u32) {
    let Ok(out) = AsyncCommand::new("pgrep").args(["-P", &pid.to_string()]).output().await else {
        return;
    };
    for child in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if let Ok(child) = child.parse::<i32>() {
            // SAFETY: kill(2) takes plain integers and touches no memory.
            unsafe {
                libc::kill(child, libc::SIGKILL);
            }
        }
    }
}

/// The browser to take cookies from for a second attempt, if one makes sense:
/// no cookies were used the first time, automatic cookies are on, the first
/// attempt hit a login wall, and a browser profile exists on disk.
pub(crate) fn cookie_retry_browser(
    auth: &[String],
    config: &Config,
    login_wall: bool,
    url: &str,
) -> Option<&'static str> {
    let have_cookies = auth
        .iter()
        .any(|a| a == "--cookies" || a == "--cookies-from-browser");
    if have_cookies || !config.cookies_auto || !login_wall {
        return None;
    }
    let browser = detect_browser();
    match browser {
        Some(b) => warn!("{url} hit a login wall, retrying with cookies from {b}"),
        None => warn!("{url} needs a login and no browser profile was found for cookies"),
    }
    browser
}

async fn run_ytdlp(
    args: &[&str],
    url: &str,
    config: &Config,
    deadline: Deadline,
) -> Result<String, BoxError> {
    let auth = ytdlp_auth_args(url, config);
    let first = spawn_ytdlp(args, &auth, url, config, deadline).await;

    // Nothing to retry with if cookies were already in play, or if the failure
    // was not a login wall in the first place.
    let Err(e) = first else { return first };
    let wall = is_auth_failure(&e.to_string());
    let Some(browser) = cookie_retry_browser(&auth, config, wall, url) else {
        return Err(e);
    };
    let mut retry = vec!["--cookies-from-browser".to_string(), browser.to_string()];
    retry.extend(auth);
    spawn_ytdlp(args, &retry, url, config, deadline)
        .await
        // Report the original failure: the retry error is usually just
        // "could not find cookies database", which is less useful.
        .map_err(|retry_err| {
            warn!("Cookie retry via {browser} also failed: {retry_err}");
            e
        })
}

async fn spawn_ytdlp(
    args: &[&str],
    extra: &[String],
    url: &str,
    config: &Config,
    deadline: Deadline,
) -> Result<String, BoxError> {
    let args: Vec<&str> = YTDLP_ONE_VIDEO.iter().chain(args).copied().collect();
    let out = run_tool("yt-dlp", &config.ytdlp_bin, &args, extra, url, deadline).await?;
    if !out.success {
        return Err(format!("yt-dlp failed ({}): {}", out.status, out.failure_detail()).into());
    }
    if is_live_skip(&out.stdout) {
        return Err(LIVE_STREAM.into());
    }
    Ok(format!("{}\n{}", out.stderr, out.stdout))
}

/// Shown for a link to a stream that is live right now.
const LIVE_STREAM: &str = "Ово је пренос уживо, то не могу да преузмем.";

/// A failure that a second, differently configured download cannot fix.
fn is_final(e: &BoxError) -> bool {
    let text = e.to_string();
    is_timeout(e) || is_no_video(&text) || text == LIVE_STREAM
}

/// Audio track selector. `language_preference>=10` is how yt-dlp marks YouTube's
/// *original* track, so this skips the auto-dubbed tracks YouTube serves by
/// default in some regions. Each fallback drops one preference at a time.
const AUDIO_FORMAT: &str = concat!(
    "ba[language_preference>=10][ext=m4a][abr<=64]/",
    "ba[language_preference>=10][ext=m4a]/",
    "ba[language_preference>=10]/",
    "ba[ext=m4a][abr<=64]/",
    "ba[ext=m4a]/",
    "ba/best"
);

/// The title, as a JSON string: one line per print however many lines the
/// title has (TikTok and Instagram captions run to several).
const TITLE_TEMPLATE: &str = "%(title)j";

/// The title printed with TITLE_TEMPLATE. A second download attempt prints
/// it again, so the last line counts. Line breaks become spaces.
fn read_title(file: &Path) -> String {
    let printed = std::fs::read_to_string(file).unwrap_or_default();
    let Some(line) = printed.lines().rev().find(|l| !l.trim().is_empty()) else {
        return String::new();
    };
    let title = serde_json::from_str::<String>(line.trim()).unwrap_or_else(|_| line.to_string());
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub async fn download_and_send_video(
    bot: &Bot,
    target: Target,
    url: &str,
    config: &Config,
) -> Result<(), BoxError> {
    let _status = show_chat_action(bot, target.chat, ChatAction::UploadVideo);
    // DOWNLOAD_TIMEOUT_SECS covers every download attempt for the link together.
    let deadline = Deadline::start(config.download_timeout);

    // An Instagram post can be photos, a video, or a carousel of both. yt-dlp
    // only sees the videos; gallery-dl fetches everything.
    let mut gallery_error = None;
    if is_instagram_post(url) {
        match send_gallery(bot, &target, url, config, deadline).await {
            Ok(()) => return Ok(()),
            Err(GalleryFailure::Unavailable) => {
                warn!("gallery-dl is not installed, so photos in Instagram posts cannot be fetched");
            }
            Err(GalleryFailure::Failed(e)) => {
                if is_timeout(&e) {
                    return Err(e);
                }
                warn!("gallery-dl failed for {url} ({e}), trying yt-dlp");
                gallery_error = Some(e);
            }
        }
    }

    match video_via_ytdlp(bot, &target, url, config, deadline).await {
        // No video at all, so most likely a photo post: send the photos.
        Err(e) if is_no_video(&e.to_string()) => match gallery_error {
            Some(gallery_error) => Err(gallery_error),
            None => photos_instead(bot, &target, url, config, deadline, e).await,
        },
        result => result,
    }
}

/// For a link yt-dlp found no video in: fetch it as a photo post instead.
async fn photos_instead(
    bot: &Bot,
    target: &Target,
    url: &str,
    config: &Config,
    deadline: Deadline,
    no_video: BoxError,
) -> Result<(), BoxError> {
    info!("No video in {url}, trying it as a photo post");
    match send_gallery(bot, target, url, config, deadline).await {
        Ok(()) => Ok(()),
        Err(GalleryFailure::Unavailable) => {
            warn!("Photos need gallery-dl, which is not installed: {no_video}");
            Err("У овом посту нема видеа, а за слике на серверу треба инсталирати gallery-dl.".into())
        }
        Err(GalleryFailure::Failed(e)) => Err(e),
    }
}

async fn video_via_ytdlp(
    bot: &Bot,
    target: &Target,
    url: &str,
    config: &Config,
    deadline: Deadline,
) -> Result<(), BoxError> {
    let tmp = TempDir::new()?;
    let ffmpeg_dir = config.ffmpeg_bin.parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| config.ffmpeg_bin.to_string_lossy().to_string());

    let budget = upload_budget(config);
    let budget_mb = human_mb(budget).max(1);
    // H.264 first, so the file usually goes out without any re-encode. The
    // audio half prefers the original track over YouTube's auto-dubs, as in
    // AUDIO_FORMAT.
    let preferred_format = format!(
        "bv[vcodec^=avc1][ext=mp4][filesize<{budget_mb}M]+ba[ext=m4a][language_preference>=10]/\
         bv[vcodec^=avc1][ext=mp4][filesize<{budget_mb}M]+ba[ext=m4a]/\
         b[vcodec^=avc1][ext=mp4][filesize<{budget_mb}M]/\
         bv[ext=mp4][filesize<{budget_mb}M]+ba[language_preference>=10][filesize<10M]/\
         bv[ext=mp4][filesize<{budget_mb}M]+ba[filesize<10M]/\
         b[ext=mp4]/b"
    );
    let max_filesize = format!("{budget_mb}M");

    // The title names the file that gets sent.
    let title_file = tmp.path().join("title.txt");
    let title_file_str = title_file.to_string_lossy().to_string();

    // --- Preferred download ---
    info!("Attempting preferred video download: {url}");
    let preferred_out = tmp.path().join("video.%(ext)s");
    let preferred_out_str = preferred_out.to_string_lossy().to_string();
    let preferred_result = run_ytdlp(
        &[
            "--ffmpeg-location", &ffmpeg_dir,
            "-f", &preferred_format,
            "--max-filesize", &max_filesize,
            "-o", &preferred_out_str,
            "--print-to-file", TITLE_TEMPLATE, &title_file_str,
            "--merge-output-format", "mp4",
            "--no-progress",
        ],
        url,
        config,
        deadline,
    )
    .await;

    let downloaded = match &preferred_result {
        Ok(output) if output.contains("Aborting") || output.contains("larger than max-filesize") => {
            warn!("Preferred download aborted (file too large)");
            None
        }
        Ok(_) => match find_prefixed_file(tmp.path(), "video.") {
            Some(p) if file_size(&p) > 1024 => {
                info!("Preferred download succeeded ({} bytes): {url}", file_size(&p));
                Some(p)
            }
            _ => {
                warn!("Preferred download produced no usable file");
                None
            }
        },
        Err(e) => {
            warn!("Preferred download failed: {e}");
            None
        }
    };

    let video_path = match downloaded {
        Some(p) => p,
        None => {
            if let Err(e) = preferred_result {
                // A timed-out first attempt leaves no time for a second, bigger
                // one, and a post with no video has nothing for it to find.
                if is_final(&e) {
                    return Err(e);
                }
            }

            // --- Fallback download: any format, converted afterwards ---
            warn!("Trying fallback download: {url}");
            let fallback_out = tmp.path().join("fallback_video.%(ext)s");
            let fallback_out_str = fallback_out.to_string_lossy().to_string();
            run_ytdlp(
                &[
                    "--ffmpeg-location", &ffmpeg_dir,
                    "-f", "bv+ba[language_preference>=10]/bv+ba/b",
                    "-o", &fallback_out_str,
                    "--print-to-file", TITLE_TEMPLATE, &title_file_str,
                    "--remux-video", "mp4",
                    "--no-warnings", "--no-progress",
                ],
                url,
                config,
                deadline,
            )
            .await?;
            find_prefixed_file(tmp.path(), "fallback_video").ok_or("Fallback file was not created")?
        }
    };

    let ready = telegram_friendly_video(&video_path, tmp.path(), config).await?;
    let fitted = fit_video(&ready, tmp.path(), config).await?;
    let title = read_title(&title_file);
    let named = named_for_sending(&fitted, tmp.path(), &title);
    send_video_file(bot, target, &named, config).await?;
    info!("Sent video: {url}");
    Ok(())
}

/// A version of the video that fits the upload limit: the file itself when it
/// already does, otherwise a smaller re-encode written to `dir`.
pub(crate) async fn fit_video(path: &Path, dir: &Path, config: &Config) -> Result<PathBuf, BoxError> {
    let budget = upload_budget(config);
    if file_size(path) <= budget {
        return Ok(path.to_path_buf());
    }

    let size = file_size(path);
    warn!(
        "Video is {}MB, over the {}MB upload limit — re-encoding smaller",
        human_mb(size),
        human_mb(config.max_upload_bytes)
    );

    let duration = media_duration(&config.ffprobe_bin, path).await.unwrap_or(0.0);
    if duration > 0.0 {
        // Budget the whole file: audio takes 64k, video gets the rest.
        let total_kbps = (budget * 8) as f64 / duration / 1000.0 * 0.95;
        let video_kbps = (total_kbps - 64.0).max(120.0) as u64;
        let shrunk = dir.join("fitted.mp4");
        let vb = format!("{video_kbps}k");
        // Cap the height too: a very low bitrate at 1080p looks worse than 480p.
        // Never upscale: a 360p source stays 360p. Even sides for x264.
        let scale = if video_kbps < 500 {
            "scale=w=-2:h='trunc(min(480,ih)/2)*2'"
        } else {
            "scale=w=-2:h='trunc(min(720,ih)/2)*2'"
        };
        let r = run_ffmpeg(
            &[
                "-format_whitelist".as_ref(), VIDEO_DEMUXERS.as_ref(),
                "-i".as_ref(), path.as_ref(),
                "-c:v".as_ref(), "libx264".as_ref(),
                "-preset".as_ref(), config.x264_preset.as_ref(),
                "-b:v".as_ref(), vb.as_ref(),
                "-pix_fmt".as_ref(), "yuv420p".as_ref(),
                "-maxrate".as_ref(), vb.as_ref(),
                "-bufsize".as_ref(), format!("{}k", video_kbps * 2).as_ref(),
                "-vf".as_ref(), scale.as_ref(),
                "-c:a".as_ref(), "aac".as_ref(),
                "-b:a".as_ref(), "64k".as_ref(),
                "-movflags".as_ref(), "+faststart".as_ref(),
                shrunk.as_ref(),
            ],
            config,
        )
        .await;

        match r {
            Ok(()) if file_size(&shrunk) > 0 && file_size(&shrunk) <= budget => {
                return Ok(shrunk);
            }
            Ok(()) => warn!("Re-encoded video still over the limit"),
            Err(e) => warn!("Shrinking video failed: {e}"),
        }
    }

    Err(format!(
        "Видео је {}MB, а лимит је {}MB. Пробај /audio.",
        human_mb(size),
        human_mb(config.max_upload_bytes)
    )
    .into())
}

pub async fn download_and_send_audio(
    bot: &Bot,
    target: Target,
    url: &str,
    config: &Config,
) -> Result<(), BoxError> {
    let _status = show_chat_action(bot, target.chat, ChatAction::UploadVoice);
    let deadline = Deadline::start(config.download_timeout);
    match audio_via_ytdlp(bot, &target, url, config, deadline).await {
        // A photo post has no sound to extract; the photos are the next best thing.
        Err(e) if is_no_video(&e.to_string()) => {
            photos_instead(bot, &target, url, config, deadline, e).await
        }
        result => result,
    }
}

async fn audio_via_ytdlp(
    bot: &Bot,
    target: &Target,
    url: &str,
    config: &Config,
    deadline: Deadline,
) -> Result<(), BoxError> {
    let tmp = TempDir::new()?;
    let audio_out = tmp.path().join("audio.%(ext)s");
    let audio_out_str = audio_out.to_string_lossy().to_string();

    info!("Downloading audio: {url}");
    let title_file = tmp.path().join("title.txt");
    let title_file_str = title_file.to_string_lossy().to_string();
    run_ytdlp(
        &[
            "-f", AUDIO_FORMAT,
            "-o", &audio_out_str,
            "--print-to-file", TITLE_TEMPLATE, &title_file_str,
            "--no-warnings", "--no-progress",
        ],
        url,
        config,
        deadline,
    )
    .await?;

    let title = read_title(&title_file);
    let title = title.as_str();

    let audio_path = find_prefixed_file(tmp.path(), "audio.")
        .ok_or("Audio file was not created")?;

    // Rename file to include the title so desktop Telegram shows it
    let ext = audio_path.extension().and_then(|e| e.to_str()).unwrap_or("m4a");
    let named_path = if title.is_empty() {
        audio_path
    } else {
        let dest = titled_path(tmp.path(), title, "audio", ext);
        match std::fs::rename(&audio_path, &dest) {
            Ok(()) => dest,
            Err(e) => {
                warn!("Could not rename audio to the video title: {e}");
                audio_path
            }
        }
    };

    let (audio, boost_db) = quiet_fallback(named_path, url, title, tmp.path(), config, deadline).await;
    deliver_audio(bot, target, &audio, title, tmp.path(), boost_db, config).await?;
    info!("Sent audio: {url}");
    Ok(())
}

/// Only for audio well below normal level. On YouTube, first fetch the louder
/// DRC stream (a second quick download, no encoding). If there is none that
/// fits, or the audio is still quiet, return the gain the encode should apply.
async fn quiet_fallback(
    path: PathBuf,
    url: &str,
    title: &str,
    dir: &Path,
    config: &Config,
    deadline: Deadline,
) -> (PathBuf, Option<f64>) {
    let Some(lufs) = sample_loudness(&path, config).await else {
        return (path, None);
    };
    if lufs >= QUIET_LUFS {
        return (path, None);
    }
    info!("Audio is quiet ({lufs:.1} LUFS)");

    let (mut path, mut lufs) = (path, lufs);
    if is_youtube(url) {
        if let Some(drc) = download_drc(url, title, dir, config, deadline).await {
            match sample_loudness(&drc, config).await {
                Some(l) => {
                    info!("Using YouTube DRC audio ({l:.1} LUFS)");
                    (path, lufs) = (drc, l);
                }
                None => info!("Using YouTube DRC audio"),
            }
            if lufs >= QUIET_LUFS {
                return (path, None);
            }
        }
    }
    (path, Some(gain_for(lufs)))
}

/// The gain that lifts audio measured at `lufs` to TARGET_LUFS.
fn gain_for(lufs: f64) -> f64 {
    let gain = (TARGET_LUFS - lufs).clamp(0.0, 30.0);
    info!("Boosting audio by {gain:.1} dB");
    gain
}

async fn download_drc(
    url: &str,
    title: &str,
    dir: &Path,
    config: &Config,
    deadline: Deadline,
) -> Option<PathBuf> {
    let drc_dir = dir.join("drc");
    std::fs::create_dir_all(&drc_dir).ok()?;
    let out = drc_dir.join("audio.%(ext)s").to_string_lossy().to_string();
    let format = drc_audio_format(upload_budget(config) / (1024 * 1024));
    info!("Downloading YouTube DRC audio");
    if let Err(e) = run_ytdlp(
        &["-f", &format, "-o", &out, "--no-warnings", "--no-progress"],
        url,
        config,
        deadline,
    )
    .await
    {
        info!("No usable DRC audio: {}", tail(&e.to_string(), 200));
        return None;
    }
    let file = find_prefixed_file(&drc_dir, "audio.")?;
    if title.is_empty() {
        return Some(file);
    }
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("m4a").to_string();
    let named = titled_path(&drc_dir, title, "audio", &ext);
    std::fs::rename(&file, &named).ok().map(|_| named).or(Some(file))
}

/// Send finished audio: as-is when it fits, re-encoded smaller when it does not,
/// and split into numbered parts when even that is not enough. `boost_db`, set
/// only for quiet audio, forces an encode with that gain.
async fn deliver_audio(
    bot: &Bot,
    target: &Target,
    path: &Path,
    title: &str,
    dir: &Path,
    boost_db: Option<f64>,
    config: &Config,
) -> Result<(), BoxError> {
    let budget = upload_budget(config);
    let duration = media_duration(&config.ffprobe_bin, path).await.unwrap_or(0.0);
    let filter = boost_db.map(boost_filter).unwrap_or_else(|| "anull".to_string());

    // Boosting means encoding anyway; never above the source bitrate (64k floor,
    // since the source may be HE-AAC), and skip it when it clearly cannot fit.
    let boost = boost_db.map(|_| {
        let source_kbps = if duration > 0.0 {
            (file_size(path) as f64 * 8.0 / duration / 1000.0).ceil() as u64
        } else {
            128
        };
        source_kbps.clamp(64, 128)
    });
    let fits = match boost {
        Some(kbps) => duration <= 0.0 || (duration * kbps as f64 * 1000.0 / 8.0 * 1.03) as u64 <= budget,
        None => true,
    };

    if fits {
        // sendAudio only plays MP3/M4A; anything else would land as a bare file.
        let converted_dir = dir.join("converted");
        std::fs::create_dir_all(&converted_dir)?;
        let converted =
            normalize_audio(path, &converted_dir, title, boost.map(|k| (filter.as_str(), k)), config).await;
        let size = file_size(&converted);
        if size <= budget {
            return send_audio_file(bot, target, &converted, title, config).await;
        }
        warn!(
            "Audio is {}MB, over the {}MB upload limit",
            human_mb(size),
            human_mb(config.max_upload_bytes)
        );
    }

    let shrunk_dir = dir.join("shrunk");
    std::fs::create_dir_all(&shrunk_dir)?;
    if let Some(shrunk) = shrink_audio(path, &shrunk_dir, title, duration, &filter, config).await? {
        return send_audio_file(bot, target, &shrunk, title, config).await;
    }

    // Too long to fit at any sane bitrate — encode at the floor, then split.
    let floor = dir.join("floor.m4a");
    run_ffmpeg(
        &[
            "-i".as_ref(), path.as_ref(),
            "-vn".as_ref(),
            "-af".as_ref(), filter.as_ref(),
            "-c:a".as_ref(), aac_encoder(config).as_ref(),
            "-b:a".as_ref(), "32k".as_ref(),
            "-ac".as_ref(), "1".as_ref(),
            "-movflags".as_ref(), "+faststart".as_ref(),
            floor.as_ref(),
        ],
        config,
    )
    .await?;

    let floor_duration = media_duration(&config.ffprobe_bin, &floor)
        .await
        .unwrap_or(duration);
    let parts = split_media(&floor, dir, floor_duration, config).await?;
    let total = parts.len();
    info!("Sending audio in {total} parts");
    for (i, part) in parts.iter().enumerate() {
        let part_title = format!("{title} ({}/{total})", i + 1);
        send_audio_file(bot, target, part, &part_title, config).await?;
    }
    Ok(())
}

#[cfg(test)]
mod titles {
    use super::*;

    #[test]
    fn titles_keep_to_one_line_and_the_last_attempt() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("title.txt");
        // Two attempts, each printing a multi-line TikTok caption as JSON.
        std::fs::write(
            &file,
            "\"first try\"\n\"Funny cat\\n\\n#fyp \\u045b\\ud83d\\udc31\"\n",
        )
        .unwrap();
        assert_eq!(read_title(&file), "Funny cat #fyp ћ🐱");
        assert_eq!(read_title(&dir.path().join("missing.txt")), "");
    }
}

#[cfg(test)]
mod tool_runs {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Whether a process is gone: no longer listed, or a zombie waiting to be reaped.
    fn is_dead(pid: &str) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .expect("ps must be available");
        let stat = String::from_utf8_lossy(&out.stdout);
        stat.trim().is_empty() || stat.trim_start().starts_with('Z')
    }

    #[test]
    fn a_timeout_also_kills_what_the_tool_started() {
        let _exec = crate::exec_lock();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(timeout_kills_children());
    }

    async fn timeout_kills_children() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("child.pid");
        let tool = dir.path().join("tool.sh");
        // Like yt-dlp handing a live stream to ffmpeg: a child that would
        // outlive its parent.
        std::fs::write(
            &tool,
            format!("#!/bin/sh\nsleep 60 &\necho $! > '{}'\nwait\n", pid_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let result = run_tool("tool", &tool, &[], &[], "url", Deadline::start(Duration::from_secs(3))).await;
        let err = result.err().expect("the run must time out");
        assert!(is_timeout(&err), "unexpected error: {err}");

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(is_dead(pid.trim()), "the tool's child is still running");
    }
}

#[cfg(test)]
mod live_check {
    use super::*;

    /// Manual smoke test for the cookie fallback, for use on the machine that
    /// actually runs the bot:
    ///
    /// ```sh
    /// TEST_URL="https://www.instagram.com/reel/..." \
    ///   cargo test -- --ignored --nocapture
    /// ```
    ///
    /// Needs network and a browser profile logged in to the site, so it is
    /// ignored by default. Pass a URL that fails when logged out.
    #[tokio::test]
    #[ignore = "needs network and a logged-in browser profile"]
    async fn login_walled_url_is_recovered_with_browser_cookies() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .try_init();
        let url = std::env::var("TEST_URL").expect("set TEST_URL to a login-walled link");
        let config = Config::from_env();

        assert!(
            detect_browser().is_some(),
            "no browser profile found to take cookies from"
        );

        let result = run_ytdlp(
            &["--no-download", "--print", "%(id)s"],
            &url,
            &config,
            Deadline::start(Duration::from_secs(90)),
        )
        .await;

        match result {
            Ok(out) => println!("recovered: {}", out.trim()),
            Err(e) => panic!("still blocked after the cookie retry: {e}"),
        }
    }
}

#[cfg(test)]
mod conversion {
    use super::*;

    fn config() -> Config {
        Config::from_env()
    }

    fn make(dir: &Path, name: &str, vcodec: &str, acodec: &str) -> PathBuf {
        // The test pattern is RGB, from which libx264 would make 4:4:4 video
        // that phones cannot play; real downloads are 4:2:0.
        make_with(dir, name, "1920x1080", 2, &["-c:v", vcodec, "-pix_fmt", "yuv420p", "-c:a", acodec])
    }

    /// A test pattern with a tone, of the given size and length, encoded with `codecs`.
    fn make_with(dir: &Path, name: &str, size: &str, secs: u32, codecs: &[&str]) -> PathBuf {
        let out = dir.join(name);
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={size}:rate=25:duration={secs}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"))
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
    async fn h264_aac_mp4_is_sent_untouched() {
        let dir = TempDir::new().unwrap();
        let src = make(dir.path(), "a.mp4", "libx264", "aac");
        let out = telegram_friendly_video(&src, dir.path(), &config()).await.unwrap();
        assert_eq!(out, src);
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn opus_audio_is_converted_but_video_is_copied() {
        let dir = TempDir::new().unwrap();
        let src = make(dir.path(), "b.mp4", "libx264", "libopus");
        let cfg = config();
        let out = telegram_friendly_video(&src, dir.path(), &cfg).await.unwrap();
        assert_ne!(out, src);
        assert_eq!(stream_codec(&cfg.ffprobe_bin, &out, "v:0").await.as_deref(), Some("h264"));
        assert_eq!(stream_codec(&cfg.ffprobe_bin, &out, "a:0").await.as_deref(), Some("aac"));
        // Copied, not rescaled.
        assert_eq!(helpers_dims(&cfg, &out).await, (Some(1920), Some(1080)));
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn vp9_webm_is_reencoded_and_capped_at_1280() {
        let dir = TempDir::new().unwrap();
        let src = make(dir.path(), "c.webm", "libvpx-vp9", "libopus");
        let cfg = config();
        let out = telegram_friendly_video(&src, dir.path(), &cfg).await.unwrap();
        assert_eq!(out.extension().unwrap(), "mp4");
        assert_eq!(stream_codec(&cfg.ffprobe_bin, &out, "v:0").await.as_deref(), Some("h264"));
        assert_eq!(helpers_dims(&cfg, &out).await, (Some(1280), Some(720)));
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn odd_sized_webm_is_converted_with_even_sides() {
        // x264 refuses odd sides; 853x480 is a common 480p WebM size.
        let dir = TempDir::new().unwrap();
        let src = make_with(dir.path(), "odd.webm", "853x480", 1, &["-c:v", "libvpx-vp9", "-c:a", "libopus"]);
        let cfg = config();
        let out = telegram_friendly_video(&src, dir.path(), &cfg).await.unwrap();
        assert_eq!(stream_codec(&cfg.ffprobe_bin, &out, "v:0").await.as_deref(), Some("h264"));
        assert_eq!(helpers_dims(&cfg, &out).await, (Some(852), Some(480)));
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn h264_that_phones_cannot_play_is_reencoded() {
        let dir = TempDir::new().unwrap();
        let src = make_with(
            dir.path(), "full-chroma.mp4", "640x360", 1,
            &["-c:v", "libx264", "-pix_fmt", "yuv444p", "-c:a", "aac"],
        );
        let cfg = config();
        let out = telegram_friendly_video(&src, dir.path(), &cfg).await.unwrap();
        assert_ne!(out, src);
        assert_eq!(stream_field(&cfg.ffprobe_bin, &out, "v:0", "pix_fmt").await.as_deref(), Some("yuv420p"));
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn rotated_phone_video_reports_its_portrait_size() {
        let dir = TempDir::new().unwrap();
        let src = make_with(
            dir.path(), "landscape.mp4", "640x360", 1,
            &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"],
        );
        let rotated = dir.path().join("rotated.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-display_rotation:v:0", "90", "-i"])
            .arg(&src)
            .args(["-c", "copy"])
            .arg(&rotated)
            .status()
            .unwrap();
        assert!(status.success());
        let cfg = config();
        assert_eq!(helpers_dims(&cfg, &rotated).await, (Some(360), Some(640)));
        // H.264 + AAC in MP4 is sent as it is, rotation flag and all.
        assert_eq!(telegram_friendly_video(&rotated, dir.path(), &cfg).await.unwrap(), rotated);
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn shrinking_an_odd_height_video_works() {
        let dir = TempDir::new().unwrap();
        // VP9, since x264 cannot even make a 4:2:0 file this size.
        let src = make_with(
            dir.path(), "big.webm", "720x405", 4,
            &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-b:v", "4M", "-c:a", "libopus"],
        );
        let mut cfg = config();
        // Room for half of it, so it has to be shrunk.
        cfg.max_upload_bytes = 512 * 1024 + file_size(&src) / 2;
        let result = fit_video(&src, dir.path(), &cfg).await;
        let fitted = dir.path().join("fitted.mp4");
        assert!(file_size(&fitted) > 0, "the shrink encode failed: {result:?}");
        let (w, h) = helpers_dims(&cfg, &fitted).await;
        assert_eq!(h, Some(404));
        assert!(w.is_some_and(|w| w % 2 == 0), "odd width {w:?}");
    }

    async fn helpers_dims(cfg: &Config, p: &Path) -> (Option<u32>, Option<u32>) {
        video_dimensions(&cfg.ffprobe_bin, p).await
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn audio_mode_never_sends_a_video_file() {
        // TikTok and clip sites have no audio-only format, so in audio mode
        // yt-dlp's "ba/.../best" falls back to the whole video.
        let dir = TempDir::new().unwrap();
        let src = make_with(
            dir.path(), "audio.mp4", "640x360", 1,
            &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"],
        );
        let cfg = config();
        let out = normalize_audio(&src, dir.path(), "song", None, &cfg).await;
        assert_ne!(out, src);
        assert_eq!(stream_codec(&cfg.ffprobe_bin, &out, "v:0").await, None);
        assert_eq!(stream_codec(&cfg.ffprobe_bin, &out, "a:0").await.as_deref(), Some("aac"));

        // Plain audio in an MP4 container still goes out as it is.
        let plain = dir.path().join("plain.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "sine=duration=1", "-c:a", "aac"])
            .arg(&plain)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(normalize_audio(&plain, dir.path(), "song", None, &cfg).await, plain);
    }
}

