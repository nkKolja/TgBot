use teloxide::prelude::*;
use teloxide::types::{InputFile, ReplyParameters};
use tokio::process::Command as AsyncCommand;
use tracing::{info, warn};
use yt_dlp::Downloader;

pub async fn get_video_duration(
    url: &str,
    downloader: &Downloader,
) -> Option<f64> {
    let video = downloader.fetch_video_infos(url).await.ok()?;
    video.duration.map(|d| d as f64)
}

/// Probe video dimensions from a downloaded file using ffmpeg.
async fn probe_dimensions(path: &std::path::Path) -> (Option<u32>, Option<u32>) {
    let output = AsyncCommand::new("libs/ffmpeg")
        .arg("-i")
        .arg(path)
        .arg("-hide_banner")
        .output()
        .await;

    let stderr = match output {
        Ok(o) => String::from_utf8_lossy(&o.stderr).to_string(),
        Err(_) => return (None, None),
    };

    for line in stderr.lines() {
        if !line.contains("Video:") {
            continue;
        }
        for token in line.split(|c: char| c == ',' || c == ' ' || c == '[') {
            let token = token.trim();
            if let Some((w_str, h_str)) = token.split_once('x') {
                if let (Ok(w), Ok(h)) = (w_str.parse::<u32>(), h_str.parse::<u32>()) {
                    if w > 0 && h > 0 && w < 10000 && h < 10000 {
                        return (Some(w), Some(h));
                    }
                }
            }
        }
    }
    (None, None)
}

/// Build a send_video request with optional width/height.
async fn send_video_with_dimensions(
    bot: &Bot,
    msg: &Message,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (w, h) = probe_dimensions(path).await;
    let mut req = bot
        .send_video(msg.chat.id, InputFile::file(path))
        .reply_parameters(ReplyParameters::new(msg.id))
        .supports_streaming(true);
    if let Some(w) = w { req = req.width(w); }
    if let Some(h) = h { req = req.height(h); }
    req.await?;
    Ok(())
}

pub async fn download_and_send_video(
    bot: &Bot,
    msg: &Message,
    url: &str,
    downloader: &Downloader,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Downloading video: {url}");

    let tmp = tempfile::TempDir::new()?;
    let out_path = tmp.path().join("video.mp4");

    match downloader.fetch_video_infos(url).await {
        Ok(video) => {
            let path = downloader.download_video_to_path(&video, &out_path).await?;
            send_video_with_dimensions(bot, msg, &path).await?;
        }
        Err(e) => {
            warn!("fetch_video_infos failed ({e}), falling back to CLI");
            cli_download(url, &out_path, false).await?;
            send_video_with_dimensions(bot, msg, &out_path).await?;
        }
    }

    info!("Sent video: {url}");
    Ok(())
}

pub async fn download_and_send_audio(
    bot: &Bot,
    msg: &Message,
    url: &str,
    downloader: &Downloader,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Downloading audio: {url}");

    let tmp = tempfile::TempDir::new()?;
    let out_path = tmp.path().join("audio.mp3");

    match downloader.fetch_video_infos(url).await {
        Ok(video) => {
            let path = downloader.download_audio_stream_to_path(&video, &out_path).await?;
            bot.send_audio(msg.chat.id, InputFile::file(&path))
                .reply_parameters(ReplyParameters::new(msg.id))
                .await?;
        }
        Err(e) => {
            warn!("fetch_video_infos failed ({e}), falling back to CLI");
            cli_download(url, &out_path, true).await?;
            bot.send_audio(msg.chat.id, InputFile::file(&out_path))
                .reply_parameters(ReplyParameters::new(msg.id))
                .await?;
        }
    }

    info!("Sent audio: {url}");
    Ok(())
}

/// CLI fallback using the yt-dlp binary installed by the crate (libs/yt-dlp).
/// Used when `fetch_video_infos` fails (e.g. X/Twitter missing `live_status`).
async fn cli_download(
    url: &str,
    out_path: &std::path::Path,
    audio_only: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let out_str = out_path.to_string_lossy();
    let mut cmd = AsyncCommand::new("libs/yt-dlp");
    cmd.arg("--ffmpeg-location").arg("libs/ffmpeg");

    if audio_only {
        cmd.args(["-x", "--audio-format", "mp3"]);
    } else {
        cmd.args([
            "-f", "bestvideo[ext=mp4]+bestaudio/best[ext=mp4]/best",
            "--merge-output-format", "mp4",
        ]);
    }

    cmd.args(["-o", &out_str, "--no-playlist", "--quiet", "--no-progress"])
        .arg(url)
        .kill_on_drop(true);

    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("yt-dlp CLI fallback failed: {stderr}").into());
    }
    Ok(())
}
