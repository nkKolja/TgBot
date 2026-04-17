use std::path::Path;
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{InputFile, ReplyParameters};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;
use tracing::{info, warn};

use crate::config::Config;
use crate::helpers::{find_prefixed_file, video_dimensions, ytdlp_auth_args};

async fn send_video_file(
    bot: &Bot,
    msg: &Message,
    path: &Path,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    req.await?;
    Ok(())
}

async fn run_ytdlp(
    args: &[&str],
    url: &str,
    config: &Config,
    timeout: Duration,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut cmd = AsyncCommand::new(&config.ytdlp_bin);
    cmd.args(args);
    for arg in ytdlp_auth_args(url, config) {
        cmd.arg(arg);
    }
    cmd.arg(url);
    cmd.kill_on_drop(true);

    let child = cmd.spawn()?;
    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;

    match result {
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if !output.status.success() {
                let detail = if !stderr.is_empty() { &stderr } else { &stdout };
                return Err(format!("yt-dlp failed (exit {}): {detail}", output.status).into());
            }
            Ok(format!("{stderr}\n{stdout}"))
        }
        Ok(Err(e)) => Err(format!("yt-dlp io error: {e}").into()),
        Err(_) => {
            Err(format!("yt-dlp cancelled: download exceeded {}s limit", timeout.as_secs()).into())
        }
    }
}

pub async fn download_and_send_video(
    bot: &Bot,
    msg: &Message,
    url: &str,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tmp = TempDir::new()?;
    let ffmpeg_dir = config.ffmpeg_bin.parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| config.ffmpeg_bin.to_string_lossy().to_string());

    // --- Preferred download ---
    info!("Attempting preferred video download: {url}");
    let preferred_out = tmp.path().join("video.%(ext)s");
    let preferred_out_str = preferred_out.to_string_lossy().to_string();
    let preferred_result = run_ytdlp(
        &[
            "--ffmpeg-location", &ffmpeg_dir,
            "-f", "bestvideo[filesize<50M][ext=mp4]+bestaudio[filesize<10M]/best[ext=mp4]/best",
            "--max-filesize", "50M",
            "-o", &preferred_out_str,
            "--merge-output-format", "mp4",
            "--postprocessor-args", "-c:v libx264 -preset fast -crf 23 -c:a aac -movflags +faststart",
            "--no-progress",
        ],
        url,
        config,
        config.download_timeout,
    )
    .await;

    if let Ok(output) = &preferred_result {
        let aborted = output.contains("Aborting") || output.contains("larger than max-filesize");
        if !aborted {
            if let Some(video_path) = find_prefixed_file(tmp.path(), "video.") {
                let file_len = video_path.metadata().map(|m| m.len()).unwrap_or(0);
                if file_len > 1024 {
                    info!("Preferred download succeeded ({file_len} bytes): {url}");
                    send_video_file(bot, msg, &video_path, config).await?;
                    info!("Sent video: {url}");
                    return Ok(());
                }
            }
        }
    }

    // --- Fallback download ---
    match &preferred_result {
        Err(e) => warn!("Preferred download failed: {e}"),
        Ok(output) if output.contains("Aborting") => warn!("Preferred download aborted (file too large)"),
        _ => warn!("Preferred download produced no usable file"),
    }
    warn!("Trying fallback download: {url}");
    let fallback_out = tmp.path().join("fallback_video");
    let fallback_out_str = fallback_out.to_string_lossy().to_string();

    run_ytdlp(
        &[
            "--ffmpeg-location", &ffmpeg_dir,
            "-f", "bestvideo+bestaudio",
            "-o", &fallback_out_str,
            "--remux-video", "mp4",
            "--no-warnings", "--no-progress",
        ],
        url,
        config,
        config.download_timeout,
    )
    .await?;

    let fallback_input = find_prefixed_file(tmp.path(), "fallback_video")
        .ok_or("Fallback file was not created")?;

    // --- Re-encode with ffmpeg ---
    let reencoded_out = tmp.path().join("reencoded.mp4");
    info!("Re-encoding fallback video: {url}");
    let reencode_status = AsyncCommand::new(&config.ffmpeg_bin)
        .args([
            "-y",
            "-i",
        ])
        .arg(&fallback_input)
        .args([
            "-c:v", "libx264",
            "-preset", "fast",
            "-crf", "24",
            "-vf", "scale=1280:-2",
            "-c:a", "aac",
            "-b:a", "128k",
            "-movflags", "+faststart",
            "-f", "mp4",
        ])
        .arg(&reencoded_out)
        .output()
        .await?;

    if !reencode_status.status.success() {
        let stderr = String::from_utf8_lossy(&reencode_status.stderr);
        return Err(format!("ffmpeg re-encode failed: {stderr}").into());
    }

    if !reencoded_out.exists() || reencoded_out.metadata().map(|m| m.len() == 0).unwrap_or(true) {
        return Err("Re-encoded file is empty".into());
    }

    send_video_file(bot, msg, &reencoded_out, config).await?;
    info!("Sent fallback/re-encoded video: {url}");
    Ok(())
}

pub async fn download_and_send_audio(
    bot: &Bot,
    msg: &Message,
    url: &str,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tmp = TempDir::new()?;
    let audio_out = tmp.path().join("audio.%(ext)s");
    let audio_out_str = audio_out.to_string_lossy().to_string();

    info!("Downloading audio: {url}");
    let title_file = tmp.path().join("title.txt");
    let title_file_str = title_file.to_string_lossy().to_string();
    run_ytdlp(
        &[
            "-f", "139/140/bestaudio[abr<=50]/bestaudio",
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
    let named_path = if !title.is_empty() {
        let safe_title: String = title.chars()
            .map(|c| if c == '/' || c == '\\' || c == ':' { '_' } else { c })
            .collect();
        let dest = tmp.path().join(format!("{safe_title}.{ext}"));
        std::fs::rename(&audio_path, &dest).unwrap_or(());
        dest
    } else {
        audio_path
    };

    let mut req = bot.send_audio(msg.chat.id, InputFile::file(&named_path));
    if !title.is_empty() {
        req = req.title(title);
    }
    if msg.chat.is_private() {
        // Don't quote in private chats
    } else {
        req = req.reply_parameters(ReplyParameters::new(msg.id));
    }
    req.await?;

    info!("Sent audio: {url}");
    Ok(())
}
