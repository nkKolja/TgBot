use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use teloxide::prelude::*;
use teloxide::types::{InputFile, ReplyParameters};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;
use tracing::{info, warn};

use crate::config::Config;
use crate::helpers::{
    detect_browser, find_prefixed_file, is_auth_failure, media_duration, video_dimensions,
    ytdlp_auth_args,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Leave room for the multipart envelope so a file that measures just under the
/// cap does not get rejected by the API.
fn upload_budget(config: &Config) -> u64 {
    config.max_upload_bytes.saturating_sub(512 * 1024)
}

fn file_size(path: &Path) -> u64 {
    path.metadata().map(|m| m.len()).unwrap_or(0)
}

fn human_mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

async fn run_ffmpeg(args: &[&std::ffi::OsStr], config: &Config) -> Result<(), BoxError> {
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

/// Time left before `deadline`, or a timeout error when it has passed.
fn remaining(deadline: Instant, total: Duration) -> Result<Duration, BoxError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left < Duration::from_secs(2) {
        return Err(format!("yt-dlp cancelled: download exceeded {}s limit", total.as_secs()).into());
    }
    Ok(left)
}

fn is_timeout(e: &BoxError) -> bool {
    e.to_string().contains("cancelled")
}

/// Codec name of the first stream of the given kind (`v:0`, `a:0`).
async fn stream_codec(ffprobe: &Path, path: &Path, selector: &str) -> Option<String> {
    let out = AsyncCommand::new(ffprobe)
        .args(["-v", "error", "-select_streams", selector])
        .args(["-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .await
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// Make a video Telegram will play inline: H.264 + AAC/MP3 in MP4. Each part
/// that is already right is copied, so most downloads cost a quick remux or
/// nothing at all; only foreign codecs (VP9, AV1, Opus) are re-encoded.
async fn telegram_friendly_video(path: &Path, dir: &Path, config: &Config) -> Result<PathBuf, BoxError> {
    let vcodec = stream_codec(&config.ffprobe_bin, path, "v:0").await;
    let acodec = stream_codec(&config.ffprobe_bin, path, "a:0").await;
    let video_ok = vcodec.as_deref() == Some("h264");
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
        "-i".into(), path.into(),
        "-map".into(), "0:v:0".into(),
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
            // Never upscale; cap the width at 1280.
            "-vf".into(), "scale=w='min(1280,iw)':h=-2".into(),
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

async fn send_document_file(
    bot: &Bot,
    msg: &Message,
    path: &Path,
    caption: Option<&str>,
) -> Result<(), BoxError> {
    let mut req = bot.send_document(msg.chat.id, InputFile::file(path.to_path_buf()));
    if let Some(caption) = caption.filter(|c| !c.is_empty()) {
        req = req.caption(caption);
    }
    if !msg.chat.is_private() {
        req = req.reply_parameters(ReplyParameters::new(msg.id));
    }
    req.await?;
    Ok(())
}

async fn send_video_file(
    bot: &Bot,
    msg: &Message,
    path: &Path,
    config: &Config,
) -> Result<(), BoxError> {
    let (w, h) = video_dimensions(&config.ffprobe_bin, path).await;

    let mut req = bot
        .send_video(msg.chat.id, InputFile::file(path.to_path_buf()))
        .supports_streaming(true);
    if !msg.chat.is_private() {
        req = req.reply_parameters(ReplyParameters::new(msg.id));
    }
    if let Some(w) = w {
        req = req.width(w);
    }
    if let Some(h) = h {
        req = req.height(h);
    }

    // Telegram rejects anything it cannot treat as a playable video (odd codec,
    // odd container). Falling back to a plain document still gets the file there.
    if let Err(e) = req.await {
        warn!("send_video failed ({e}), retrying as document");
        send_document_file(bot, msg, path, None).await?;
    }
    Ok(())
}

async fn send_audio_file(
    bot: &Bot,
    msg: &Message,
    path: &Path,
    title: &str,
    config: &Config,
) -> Result<(), BoxError> {
    let mut req = bot.send_audio(msg.chat.id, InputFile::file(path.to_path_buf()));
    if !title.is_empty() {
        req = req.title(title);
    }
    if let Some(secs) = media_duration(&config.ffprobe_bin, path).await {
        req = req.duration(secs as u32);
    }
    if !msg.chat.is_private() {
        req = req.reply_parameters(ReplyParameters::new(msg.id));
    }

    // sendAudio only accepts MP3/M4A; anything else (or an oversized upload)
    // errors out, and a document keeps the file usable either way.
    if let Err(e) = req.await {
        warn!("send_audio failed ({e}), retrying as document");
        send_document_file(bot, msg, path, Some(title)).await?;
    }
    Ok(())
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
    // Filesystems cap names at 255 bytes; Cyrillic titles hit that sooner than
    // the character count suggests.
    let stem: String = stem.chars().take(120).collect();
    dir.join(format!("{stem}.{ext}"))
}

/// Re-encode audio at whatever bitrate fits the upload budget. Returns `None`
/// when even the floor bitrate would not fit, in which case the caller splits.
async fn shrink_audio(
    src: &Path,
    dir: &Path,
    title: &str,
    duration: f64,
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
            "-c:a".as_ref(), "aac".as_ref(),
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

/// Remux to M4A when the container is something Telegram will not play as
/// audio (opus/webm, for instance). Returns the original path if it is already
/// fine or if the remux fails.
async fn normalize_audio(src: &Path, dir: &Path, title: &str, config: &Config) -> PathBuf {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "mp3" | "m4a" | "mp4" | "aac") {
        return src.to_path_buf();
    }

    let dest = titled_path(dir, title, "audio", "m4a");
    info!("Converting .{ext} audio to m4a for Telegram");
    match run_ffmpeg(
        &[
            "-i".as_ref(), src.as_ref(),
            "-vn".as_ref(),
            "-c:a".as_ref(), "aac".as_ref(),
            "-b:a".as_ref(), "128k".as_ref(),
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
fn tail(text: &str, max_chars: usize) -> String {
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

// --- yt-dlp ------------------------------------------------------------------

async fn run_ytdlp(
    args: &[&str],
    url: &str,
    config: &Config,
    timeout: Duration,
) -> Result<String, BoxError> {
    let auth = ytdlp_auth_args(url, config);
    let have_cookies = auth
        .iter()
        .any(|a| a == "--cookies" || a == "--cookies-from-browser");

    let first = spawn_ytdlp(args, &auth, url, config, timeout).await;

    // Nothing to retry with if cookies were already in play, or if the failure
    // was not a login wall in the first place.
    let Err(e) = first else { return first };
    if have_cookies || !config.cookies_auto || !is_auth_failure(&e.to_string()) {
        return Err(e);
    }
    let Some(browser) = detect_browser() else {
        warn!("{url} needs a login and no browser profile was found for cookies");
        return Err(e);
    };

    warn!("{url} hit a login wall, retrying with cookies from {browser}");
    let mut retry = vec!["--cookies-from-browser".to_string(), browser.to_string()];
    retry.extend(auth);
    spawn_ytdlp(args, &retry, url, config, timeout)
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
    timeout: Duration,
) -> Result<String, BoxError> {
    let mut cmd = AsyncCommand::new(&config.ytdlp_bin);
    cmd.args(args);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.arg(url);
    cmd.kill_on_drop(true);
    // Without explicit pipes the child inherits our stdio and wait_with_output()
    // hands back empty buffers — which silently blinds every check we make on
    // yt-dlp's output below.
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let child = cmd.spawn()?;
    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;

    match result {
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if !output.status.success() {
                let detail = if stderr.trim().is_empty() { &stdout } else { &stderr };
                return Err(
                    format!("yt-dlp failed ({}): {}", output.status, tail(detail, 1200)).into(),
                );
            }
            Ok(format!("{stderr}\n{stdout}"))
        }
        Ok(Err(e)) => Err(format!("yt-dlp io error: {e}").into()),
        Err(_) => {
            Err(format!("yt-dlp cancelled: download exceeded {}s limit", timeout.as_secs()).into())
        }
    }
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

pub async fn download_and_send_video(
    bot: &Bot,
    msg: &Message,
    url: &str,
    config: &Config,
) -> Result<(), BoxError> {
    let tmp = TempDir::new()?;
    let ffmpeg_dir = config.ffmpeg_bin.parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| config.ffmpeg_bin.to_string_lossy().to_string());

    // DOWNLOAD_TIMEOUT_SECS covers both download attempts together, not each.
    let total = config.download_timeout;
    let deadline = Instant::now() + total;

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
            "--merge-output-format", "mp4",
            "--no-progress",
        ],
        url,
        config,
        remaining(deadline, total)?,
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
            // A timed-out first attempt leaves no time for a second, bigger one.
            if let Err(e) = preferred_result {
                if is_timeout(&e) {
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
                    "--remux-video", "mp4",
                    "--no-warnings", "--no-progress",
                ],
                url,
                config,
                remaining(deadline, total)?,
            )
            .await?;
            find_prefixed_file(tmp.path(), "fallback_video").ok_or("Fallback file was not created")?
        }
    };

    let ready = telegram_friendly_video(&video_path, tmp.path(), config).await?;
    deliver_video(bot, msg, &ready, tmp.path(), config).await?;
    info!("Sent video: {url}");
    Ok(())
}

/// Send a finished video, squeezing it under the upload limit first if needed.
async fn deliver_video(
    bot: &Bot,
    msg: &Message,
    path: &Path,
    dir: &Path,
    config: &Config,
) -> Result<(), BoxError> {
    let budget = upload_budget(config);
    if file_size(path) <= budget {
        return send_video_file(bot, msg, path, config).await;
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
        // Never upscale: a 360p source stays 360p.
        let scale = if video_kbps < 500 {
            "scale=w=-2:h='min(480,ih)'"
        } else {
            "scale=w=-2:h='min(720,ih)'"
        };
        let r = run_ffmpeg(
            &[
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
                return send_video_file(bot, msg, &shrunk, config).await;
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
    msg: &Message,
    url: &str,
    config: &Config,
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
            "--print-to-file", "title", &title_file_str,
            "--no-warnings", "--no-progress",
        ],
        url,
        config,
        config.download_timeout,
    )
    .await?;

    let title = std::fs::read_to_string(&title_file).unwrap_or_default();
    let title = title.trim();

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

    deliver_audio(bot, msg, &named_path, title, tmp.path(), config).await?;
    info!("Sent audio: {url}");
    Ok(())
}

/// Send finished audio: as-is when it fits, re-encoded smaller when it does not,
/// and split into numbered parts when even that is not enough.
async fn deliver_audio(
    bot: &Bot,
    msg: &Message,
    path: &Path,
    title: &str,
    dir: &Path,
    config: &Config,
) -> Result<(), BoxError> {
    // sendAudio only plays MP3/M4A; anything else would land as a bare file.
    let converted_dir = dir.join("converted");
    std::fs::create_dir_all(&converted_dir)?;
    let path = &normalize_audio(path, &converted_dir, title, config).await;

    let budget = upload_budget(config);
    let size = file_size(path);
    if size <= budget {
        return send_audio_file(bot, msg, path, title, config).await;
    }

    warn!(
        "Audio is {}MB, over the {}MB upload limit",
        human_mb(size),
        human_mb(config.max_upload_bytes)
    );
    let duration = media_duration(&config.ffprobe_bin, path).await.unwrap_or(0.0);

    let shrunk_dir = dir.join("shrunk");
    std::fs::create_dir_all(&shrunk_dir)?;
    if let Some(shrunk) = shrink_audio(path, &shrunk_dir, title, duration, config).await? {
        return send_audio_file(bot, msg, &shrunk, title, config).await;
    }

    // Too long to fit at any sane bitrate — encode at the floor, then split.
    let floor = dir.join("floor.m4a");
    run_ffmpeg(
        &[
            "-i".as_ref(), path.as_ref(),
            "-vn".as_ref(),
            "-c:a".as_ref(), "aac".as_ref(),
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
        send_audio_file(bot, msg, part, &part_title, config).await?;
    }
    Ok(())
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
            Duration::from_secs(90),
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
        let out = dir.join(name);
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i", "testsrc=size=1920x1080:rate=25:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(["-c:v", vcodec, "-c:a", acodec, "-shortest"])
            .arg(&out)
            .status()
            .expect("ffmpeg must be installed for this test");
        assert!(status.success());
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

    async fn helpers_dims(cfg: &Config, p: &Path) -> (Option<u32>, Option<u32>) {
        video_dimensions(&cfg.ffprobe_bin, p).await
    }
}
