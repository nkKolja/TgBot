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
    "fb.watch",
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

/// True when `host` is `domain` itself or one of its subdomains, so that
/// `x.com` matches `x.com` and `mobile.x.com` but not `dropbox.com`.
pub fn host_matches(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// Parse one candidate token as a link to a supported site. Accepts links with
/// or without a scheme (`youtu.be/abc`), strips punctuation that commonly
/// clings to a link in prose, and rejects bare domains with no path, which
/// cannot point at any media.
pub fn supported_url(candidate: &str) -> Option<String> {
    let token = candidate.trim_matches(|c: char| {
        matches!(c, '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | ',' | '.' | '!' | '?' | ';' | '«' | '»')
    });
    if token.is_empty() {
        return None;
    }
    let lower = token.to_ascii_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        token.to_string()
    } else if token.contains("://") {
        return None;
    } else {
        format!("https://{token}")
    };
    let url = url::Url::parse(&with_scheme).ok()?;
    let host = url.host_str()?;
    if !SUPPORTED_DOMAINS.iter().any(|d| host_matches(host, d)) {
        return None;
    }
    if url.path().len() <= 1 && url.query().is_none() {
        return None;
    }
    Some(url.into())
}

/// An Instagram post link: `/p/<code>`, also in the `/<user>/p/<code>` and
/// `/share/p/<code>` forms. Reels and IGTV links are videos and stay with yt-dlp.
pub fn is_instagram_post(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else { return false };
    if !url.host_str().is_some_and(|h| host_matches(h, "instagram.com")) {
        return false;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    matches!(segments.as_slice(), ["p", _, ..] | [_, "p", _, ..])
}

/// Whether a yt-dlp failure means the link has no video at all — usually a
/// photo post, which gallery-dl can fetch instead.
pub fn is_no_video(output: &str) -> bool {
    let lower = output.to_lowercase();
    [
        "no video formats found",
        "there is no video",
        "no video could be found",
        "no video in this",
        "does not contain any video",
        "no media found",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Whether a gallery-dl failure looks like a login wall. gallery-dl reports
/// problems as "[instagram][error] HTTP redirect to login page (...)" or
/// "[twitter][error] AuthRequired: 'authenticated cookies' needed to access ...".
pub fn is_gallery_auth_failure(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower
        .lines()
        .filter(|line| line.contains("][error]") || line.contains("][warning]"))
        .any(|line| {
            [
                "redirect to login",
                "redirect to challenge",
                "login required",
                "authrequired",
                "authorizationerror",
                "authenticationerror",
                "needed to access",
                "401 unauthorized",
                "403 forbidden",
                "is private",
                "cookies",
            ]
            .iter()
            .any(|needle| line.contains(needle))
        })
}

/// First link to a supported site anywhere in a message's text.
pub fn extract_supported_url(text: &str) -> Option<String> {
    text.split_whitespace().find_map(supported_url)
}

/// Width and height as the video is shown. A phone stores a portrait video
/// as landscape frames plus a 90° rotation flag, and Telegram sizes its player
/// from the numbers it is given, so those are swapped back.
fn displayed_dimensions(probe: &str) -> Option<(Option<u32>, Option<u32>)> {
    let (mut width, mut height, mut rotation) = (None, None, 0.0_f64);
    for line in probe.lines() {
        match line.split_once('=') {
            Some(("width", v)) => width = v.trim().parse().ok(),
            Some(("height", v)) => height = v.trim().parse().ok(),
            // ffmpeg 5+ reports a display matrix; older versions a rotate tag.
            Some(("rotation" | "TAG:rotate", v)) => rotation = v.trim().parse().unwrap_or(0.0),
            _ => {}
        }
    }
    if width.is_none() && height.is_none() {
        return None;
    }
    if (rotation.round() as i64).rem_euclid(180) == 90 {
        std::mem::swap(&mut width, &mut height);
    }
    Some((width, height))
}

pub async fn video_dimensions(ffprobe: &Path, path: &Path) -> (Option<u32>, Option<u32>) {
    // Try ffprobe first, fall back to ffmpeg -i if ffprobe is not available
    // "V": the video itself, not cover art stored as a picture stream.
    let out = AsyncCommand::new(ffprobe)
        .args(["-v", "error", "-select_streams", "V:0", "-show_entries"])
        .arg("stream=width,height:stream_side_data=rotation:stream_tags=rotate")
        .args(["-of", "default=nw=1"])
        .arg(path)
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            if let Some(dimensions) = displayed_dimensions(&String::from_utf8_lossy(&o.stdout)) {
                return dimensions;
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
            for token in line.split([',', ' ', '[']) {
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
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(prefix))
                .unwrap_or(false)
        })
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
/// error. Most login-gated failures carry yt-dlp's "Use --cookies..." hint, but
/// some extractors pass the site's own wording through untouched.
pub fn is_auth_failure(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower
        .lines()
        // yt-dlp prints this warning whenever *_USERNAME is set for a site with
        // no password login. It mentions --cookies but says nothing about why
        // this particular download failed, so it must not trigger a retry.
        .filter(|line| !line.contains("login with password is not supported"))
        .any(|line| {
            [
                "--cookies",
                "age-restricted",
                "age restricted",
                "sign in to confirm",
                "log in to continue",
                "login required",
                "private video",
                "members-only",
                // Instagram passes its own gating text through verbatim.
                "available to everyone",
                "certain audiences",
                "only available for registered users",
                "restricted video",
                "content is unreachable",
                "need to log in",
            ]
            .iter()
            .any(|needle| line.contains(needle))
        })
}

/// Turn an internal error into something short enough to show in the chat.
/// Raw yt-dlp output can run to a kilobyte of tracebacks and temp paths; the
/// user only needs its final ERROR line.
pub fn user_facing_error(err: &str) -> String {
    const MAX_CHARS: usize = 300;
    let lower = err.to_lowercase();
    let detail = if lower.contains("encode exceeded") {
        "Обрада је трајала предуго.".to_string()
    } else if lower.contains("cancelled") && lower.contains("limit") {
        "Преузимање је трајало предуго.".to_string()
    } else if let Some(line) = err.lines().rev().find(|l| l.contains("ERROR:")) {
        line.split_once("ERROR:").map(|(_, r)| r.trim().to_string()).unwrap_or_default()
    } else if let Some(line) = err.lines().find(|l| l.contains("][error] ")) {
        // gallery-dl: the first error is the cause; later ones are fallout
        // such as "Failed to download ...".
        line.split_once("][error] ").map(|(_, r)| r.trim().to_string()).unwrap_or_default()
    } else {
        err.lines().next().unwrap_or("").trim().to_string()
    };
    let mut detail: String = detail.chars().take(MAX_CHARS).collect();
    if detail.chars().count() == MAX_CHARS {
        detail.push('…');
    }
    format!("Грешка: {detail}")
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

/// Passed to every yt-dlp run. A YouTube link copied from a playlist or a Mix
/// (`watch?v=…&list=…`) otherwise downloads the whole list, and a live stream
/// would be recorded until the timeout, by an ffmpeg that outlives yt-dlp.
pub const YTDLP_ONE_VIDEO: &[&str] = &[
    "--no-playlist",
    "--playlist-items", "1",
    "--match-filter", "!is_live",
];

/// What yt-dlp prints when `!is_live` turns a link down.
pub fn is_live_skip(output: &str) -> bool {
    output.contains("does not pass filter (!is_live)")
}

/// Get video duration in seconds using yt-dlp extract_info (no download).
pub async fn get_video_duration(url: &str, config: &Config) -> Option<f64> {
    let mut cmd = AsyncCommand::new(&config.ytdlp_bin);
    if let Some(path) = crate::config::tool_path() {
        cmd.env("PATH", path);
    }
    cmd.args(YTDLP_ONE_VIDEO);
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
        // Instagram's own wording, with no cookie hint attached.
        assert!(is_auth_failure(
            "ERROR: [Instagram] DcExxxnBMGa: This content isn't available to everyone: \
             It can't be seen by certain audiences."
        ));
    }

    #[test]
    fn the_password_warning_alone_does_not_trigger_a_retry() {
        // Emitted whenever *_USERNAME is set, on every failure, relevant or not.
        assert!(!is_auth_failure(
            "WARNING: [Instagram] Login with password is not supported for this website. \
             Use --cookies-from-browser or --cookies for the authentication.\n\
             ERROR: [Instagram] abc: Video unavailable. This post has been deleted"
        ));
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

    #[test]
    fn supported_links_are_found_inside_prose() {
        assert_eq!(
            extract_supported_url("look at this https://youtu.be/dQw4w9WgXcQ haha").as_deref(),
            Some("https://youtu.be/dQw4w9WgXcQ")
        );
        assert_eq!(
            extract_supported_url("(www.instagram.com/reel/C123/),").as_deref(),
            Some("https://www.instagram.com/reel/C123/")
        );
        assert_eq!(
            extract_supported_url("https://mobile.x.com/user/status/1").as_deref(),
            Some("https://mobile.x.com/user/status/1")
        );
    }

    #[test]
    fn lookalike_and_bare_domains_are_rejected() {
        assert_eq!(extract_supported_url("https://www.dropbox.com/s/abc/file.mp4"), None);
        assert_eq!(extract_supported_url("https://box.com/v/abc"), None);
        assert_eq!(extract_supported_url("https://notyoutube.com/watch?v=1"), None);
        assert_eq!(extract_supported_url("https://youtube.com.evil.io/watch?v=1"), None);
        assert_eq!(extract_supported_url("I use instagram.com a lot"), None);
        assert_eq!(extract_supported_url("ftp://youtube.com/watch"), None);
        assert_eq!(extract_supported_url("hello"), None);
    }

    #[test]
    fn host_matching_is_label_aligned() {
        assert!(host_matches("x.com", "x.com"));
        assert!(host_matches("WWW.X.COM", "x.com"));
        assert!(host_matches("www.x.com.", "x.com"));
        assert!(!host_matches("dropbox.com", "x.com"));
        assert!(!host_matches("x.com.evil.io", "x.com"));
    }

    #[test]
    fn user_facing_errors_keep_only_the_relevant_line() {
        let raw = "yt-dlp failed (exit status: 1): WARNING: something\n\
                   [debug] /var/folders/xy/T/.tmpAbc/video.%(ext)s\n\
                   ERROR: [youtube] abc: Video unavailable";
        assert_eq!(user_facing_error(raw), "Грешка: [youtube] abc: Video unavailable");
        assert_eq!(
            user_facing_error("yt-dlp cancelled: download exceeded 60s limit"),
            "Грешка: Преузимање је трајало предуго."
        );
        assert!(user_facing_error(&"x".repeat(5000)).chars().count() < 320);
    }

    #[test]
    fn rotated_phone_videos_report_their_displayed_size() {
        let shown = |probe: &str| displayed_dimensions(probe);
        assert_eq!(shown("width=1920\nheight=1080\nrotation=-90\n"), Some((Some(1080), Some(1920))));
        assert_eq!(shown("width=1920\nheight=1080\nTAG:rotate=270\n"), Some((Some(1080), Some(1920))));
        assert_eq!(shown("width=1920\nheight=1080\nrotation=180\n"), Some((Some(1920), Some(1080))));
        assert_eq!(shown("width=640\nheight=360\n"), Some((Some(640), Some(360))));
        assert_eq!(shown(""), None);
    }

    #[test]
    fn slow_encodes_are_not_blamed_on_the_download() {
        assert_eq!(
            user_facing_error("re-encode failed: ffmpeg cancelled: encode exceeded 600s limit"),
            "Грешка: Обрада је трајала предуго."
        );
    }

    #[test]
    fn instagram_posts_are_told_apart_from_reels() {
        assert!(is_instagram_post("https://www.instagram.com/p/C3xYz_AbCd/"));
        assert!(is_instagram_post("https://instagram.com/p/C3xYz_AbCd/?igsh=abc"));
        assert!(is_instagram_post("https://www.instagram.com/someone/p/C3xYz_AbCd/"));
        assert!(is_instagram_post("https://www.instagram.com/share/p/BAxYz/"));
        assert!(!is_instagram_post("https://www.instagram.com/reel/C3xYz_AbCd/"));
        assert!(!is_instagram_post("https://www.instagram.com/tv/C3xYz_AbCd/"));
        assert!(!is_instagram_post("https://www.instagram.com/someone/"));
        assert!(!is_instagram_post("https://www.instagram.com/p/"));
        assert!(!is_instagram_post("https://example.com/p/C3xYz_AbCd/"));
    }

    #[test]
    fn photo_posts_are_recognised_from_ytdlp_errors() {
        assert!(is_no_video("ERROR: [Instagram] C3x: No video formats found!"));
        assert!(is_no_video("ERROR: [Instagram] C3x: There is no video in this post"));
        assert!(is_no_video("ERROR: [twitter] 123: No video could be found in this tweet"));
        assert!(!is_no_video("ERROR: [youtube] abc: Video unavailable"));
        assert!(!is_no_video("yt-dlp cancelled: download exceeded the 60s limit"));
    }

    #[test]
    fn gallery_dl_login_walls_are_recognised() {
        assert!(is_gallery_auth_failure(
            "[instagram][error] HTTP redirect to login page (https://www.instagram.com/accounts/login/)"
        ));
        assert!(is_gallery_auth_failure(
            "[instagram][error] HttpError: '401 Unauthorized' for 'https://www.instagram.com/api/v1/media/1/info/'"
        ));
        assert!(is_gallery_auth_failure(
            "[twitter][error] AuthRequired: 'authenticated cookies' needed to access this timeline"
        ));
        assert!(!is_gallery_auth_failure(
            "[downloader.http][warning] '404 File not found' for 'https://x/a.jpg'\n\
             [download][error] Failed to download a.jpg"
        ));
        // Info lines mentioning a login are not failures.
        assert!(!is_gallery_auth_failure("[instagram][info] Logging in as someone"));
    }

    #[test]
    fn gallery_dl_errors_show_the_cause() {
        let raw = "gallery-dl failed (exit status: 4): \
                   [instagram][error] HTTP redirect to login page (https://www.instagram.com/accounts/login/)\n\
                   [download][error] Failed to download x.jpg";
        assert_eq!(
            user_facing_error(raw),
            "Грешка: HTTP redirect to login page (https://www.instagram.com/accounts/login/)"
        );
    }
}
