use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rand::seq::SliceRandom;
use tokio::process::Command as AsyncCommand;
use tracing::{error, warn};

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
    // Try ffprobe first, fall back to ffmpeg -i if ffprobe is not available
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
                return (parts[0].parse().ok(), parts[1].parse().ok());
            }
        }
        Ok(o) => {
            error!("ffprobe error: {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(_) => {
            // ffprobe not found — try ffmpeg -i instead
        }
    }

    // Fallback: parse dimensions from ffmpeg -i stderr
    let ffmpeg = ffprobe.with_file_name("ffmpeg");
    let out = AsyncCommand::new(&ffmpeg)
        .arg("-i")
        .arg(path)
        .arg("-hide_banner")
        .output()
        .await;

    if let Ok(o) = out {
        let stderr = String::from_utf8_lossy(&o.stderr);
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
    }

    (None, None)
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
    let creds = config.services.for_url(url);

    // Cookies first: a per-service jar wins over the shared one, which wins over
    // pulling them straight out of a browser profile.
    let cookie_file = creds
        .and_then(|c| c.cookies.as_ref())
        .or(config.cookies_file.as_ref());
    if let Some(path) = cookie_file {
        if path.exists() {
            args.push("--cookies".into());
            args.push(path.to_string_lossy().into_owned());
        } else {
            warn!("Cookie file {} does not exist, ignoring", path.display());
        }
    } else if let Some(browser) = &config.cookies_from_browser {
        args.push("--cookies-from-browser".into());
        args.push(browser.clone());
    }

    // yt-dlp has no password login for any site this bot supports — it only warns
    // and carries on — but pass it through in case a future extractor gains one.
    if let Some(creds) = creds {
        if let (Some(u), Some(p)) = (&creds.username, &creds.password) {
            args.push("--username".into());
            args.push(u.clone());
            args.push("--password".into());
            args.push(p.clone());
        }
    }
    args
}

/// Browsers yt-dlp can pull cookies out of, best first. Firefox leads because
/// its jar is plain SQLite in the home directory; Chromium browsers need macOS
/// Keychain access, which a launchd service normally cannot get.
const BROWSER_PROFILES: &[(&str, &str)] = &[
    ("firefox", "Library/Application Support/Firefox"),
    ("chrome", "Library/Application Support/Google/Chrome"),
    ("brave", "Library/Application Support/BraveSoftware/Brave-Browser"),
    ("edge", "Library/Application Support/Microsoft Edge"),
    ("vivaldi", "Library/Application Support/Vivaldi"),
    ("chromium", "Library/Application Support/Chromium"),
];

/// First browser with a profile on disk, for `--cookies-from-browser`.
pub fn detect_browser() -> Option<&'static str> {
    static DETECTED: OnceLock<Option<&'static str>> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        BROWSER_PROFILES
            .iter()
            .find(|(_, dir)| home.join(dir).is_dir())
            .map(|(name, _)| *name)
    })
}

/// Whether a yt-dlp failure looks like a login or age wall rather than a real
/// error. yt-dlp appends a "Use --cookies..." hint to exactly these.
pub fn is_auth_failure(output: &str) -> bool {
    let lower = output.to_lowercase();
    [
        "--cookies",
        "age-restricted",
        "age restricted",
        "sign in to confirm",
        "log in to continue",
        "login required",
        "private video",
        "members-only",
        "requested content is not available",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Duration in seconds of an already-downloaded media file.
pub async fn media_duration(ffprobe: &Path, path: &Path) -> Option<f64> {
    let out = AsyncCommand::new(ffprobe)
        .args([
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "csv=p=0",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().ok()
}

/// Get video duration in seconds using yt-dlp extract_info (no download).
pub async fn get_video_duration(url: &str, config: &Config) -> Option<f64> {
    let mut cmd = AsyncCommand::new(&config.ytdlp_bin);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_walls_are_recognised() {
        // Real yt-dlp output: every login-gated failure carries the cookie hint.
        assert!(is_auth_failure(
            "ERROR: [Instagram] C123: Instagram sent an empty media response. \
             ... then use --cookies-from-browser or --cookies for the authentication."
        ));
        assert!(is_auth_failure(
            "ERROR: [youtube] abc: Sign in to confirm your age. This video may be inappropriate."
        ));
        assert!(is_auth_failure("This video is age-restricted"));
    }

    #[test]
    fn ordinary_failures_are_not_retried() {
        assert!(!is_auth_failure(
            "yt-dlp cancelled: download exceeded 60s limit"
        ));
        assert!(!is_auth_failure(
            "ERROR: [youtube] abc: Video unavailable. This video has been removed by the uploader"
        ));
        assert!(!is_auth_failure("ffmpeg failed: no such file or directory"));
    }
}
