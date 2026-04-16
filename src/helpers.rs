use std::path::{Path, PathBuf};

use rand::seq::SliceRandom;
use tokio::process::Command as AsyncCommand;
use tracing::error;

use crate::config::Config;

const SUPPORTED_DOMAINS: &[&str] = &[
    "youtu.be",
    "facebook.com",
    "x.com",
    "twitter.com",
    "tiktok.com",
    "instagram.com",
    "youtube.com",
    "reddit.com",
    "linkedin.com",
];

const GREETINGS: &[&str] = &[
    "краљу",
    "баки",
    "царе",
    "легендице",
    "друже",
    "чоче",
    "јадо",
    "мајсторе",
    "легендо",
    "брате",
    "геније",
    "душо",
    "сине",
    "мангупе",
    "фрајеру",
    "братко",
    "батко",
    "срце",
    "пријатељу",
    "комшо",
    "комшија",
];

pub fn random_greeting() -> &'static str {
    GREETINGS
        .choose(&mut rand::thread_rng())
        .copied()
        .unwrap_or("друже")
}

pub fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(ch) => ch.to_uppercase().collect::<String>() + c.as_str(),
    }
}

pub fn is_supported_url(text: &str) -> bool {
    SUPPORTED_DOMAINS.iter().any(|d| text.contains(d))
}

pub async fn video_dimensions(ffprobe: &Path, path: &Path) -> (Option<u32>, Option<u32>) {
    let out = AsyncCommand::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let parts: Vec<&str> = s.trim().split(',').collect();
            if parts.len() == 2 {
                (parts[0].parse().ok(), parts[1].parse().ok())
            } else {
                (None, None)
            }
        }
        Ok(o) => {
            error!("ffprobe error: {}", String::from_utf8_lossy(&o.stderr));
            (None, None)
        }
        Err(e) => {
            error!("ffprobe failed to run: {e}");
            (None, None)
        }
    }
}

pub fn find_prefixed_file(dir: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(prefix))
                .unwrap_or(false)
        })
        .next()
}

pub fn ytdlp_auth_args(url: &str, config: &Config) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(creds) = config.services.for_url(url) {
        if let (Some(u), Some(p)) = (&creds.username, &creds.password) {
            args.push("--username".into());
            args.push(u.clone());
            args.push("--password".into());
            args.push(p.clone());
        }
    }
    args
}

/// Get video duration in seconds using yt-dlp extract_info (no download).
pub async fn get_video_duration(url: &str, config: &Config) -> Option<f64> {
    let mut cmd = AsyncCommand::new("yt-dlp");
    cmd.args(["--print", "duration", "--no-download", "--no-warnings", "--quiet"]);
    for arg in ytdlp_auth_args(url, config) {
        cmd.arg(arg);
    }
    cmd.arg(url);
    cmd.kill_on_drop(true);

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output()).await;
    match result {
        Ok(Ok(output)) if output.status.success() => {
            let s = String::from_utf8_lossy(&output.stdout);
            s.trim().parse::<f64>().ok()
        }
        _ => None,
    }
}
