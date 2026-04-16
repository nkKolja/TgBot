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
        .reply_parameters(ReplyParameters::new(msg.id))
        .supports_streaming(true);
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut cmd = AsyncCommand::new("yt-dlp");
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
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("yt-dlp failed: {stderr}").into());
            }
            Ok(())
        }
        Ok(Err(e)) => Err(format!("yt-dlp io error: {e}").into()),
        Err(_) => {
            // timeout — kill_on_drop handles cleanup when child is dropped here
            Err(format!("yt-dlp timed out after {}s", timeout.as_secs()).into())
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
    let video_out = tmp.path().join("video.mp4");
    let video_out_str = video_out.to_string_lossy().to_string();
    let ffmpeg_str = config.ffmpeg_bin.to_string_lossy().to_string();

    // --- Preferred download ---
    info!("Attempting preferred video download: {url}");
    let preferred_result = run_ytdlp(
        &[
            "--ffmpeg-location", &ffmpeg_str,
            "-f", "bestvideo[filesize<50M][ext=mp4]+bestaudio[filesize<10M]/best[ext=mp4]/best",
            "--max-filesize", "50M",
            "-o", &video_out_str,
            "--merge-output-format", "mp4",
            "--postprocessor-args", "-c:v libx264 -preset fast -crf 23 -c:a aac -movflags +faststart",
            "--quiet", "--no-progress",
        ],
        url,
        config,
        config.download_timeout,
    )
    .await;

    if preferred_result.is_ok() && video_out.exists() && video_out.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        info!("Preferred download succeeded: {url}");
        send_video_file(bot, msg, &video_out, config).await?;
        info!("Sent video: {url}");
        return Ok(());
    }

    // --- Fallback download ---
    warn!("Preferred download failed, trying fallback: {url}");
    let fallback_out = tmp.path().join("fallback_video");
    let fallback_out_str = fallback_out.to_string_lossy().to_string();

    run_ytdlp(
        &[
            "--ffmpeg-location", &ffmpeg_str,
            "-f", "bestvideo+bestaudio",
            "-o", &fallback_out_str,
            "--remux-video", "mp4",
            "--quiet", "--no-progress",
        ],
        url,
        config,
        config.download_timeout,
    )
    .await?;

    let fallback_input = find_prefixed_file(tmp.path(), "fallback_video")
        .ok_or("Fallback file was not created")?;

    // --- Re-encode with ffmpeg ---
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
        .arg(&video_out)
        .output()
        .await?;

    if !reencode_status.status.success() {
        let stderr = String::from_utf8_lossy(&reencode_status.stderr);
        return Err(format!("ffmpeg re-encode failed: {stderr}").into());
    }

    if !video_out.exists() || video_out.metadata().map(|m| m.len() == 0).unwrap_or(true) {
        return Err("Re-encoded file is empty".into());
    }

    send_video_file(bot, msg, &video_out, config).await?;
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
    run_ytdlp(
        &[
            "-f", "139/140/bestaudio[abr<=50]/bestaudio",
            "-o", &audio_out_str,
            "--quiet", "--no-progress",
        ],
        url,
        config,
        config.download_timeout,
    )
    .await?;

    let audio_path = find_prefixed_file(tmp.path(), "audio.")
        .ok_or("Audio file was not created")?;

    bot.send_audio(msg.chat.id, InputFile::file(audio_path))
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;

    info!("Sent audio: {url}");
    Ok(())
}
