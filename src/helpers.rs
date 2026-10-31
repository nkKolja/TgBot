use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rand::seq::SliceRandom;
use tokio::process::Command as AsyncCommand;
use tracing::{error, warn};

use crate::config::Config;

/// Sites the bot downloads from, subdomains included. Links anywhere else are
/// ignored, so in groups the bot only speaks up for media links.
const SUPPORTED_DOMAINS: &[&str] = &[
    "youtu.be",
    "facebook.com",
    "x.com",
    "twitter.com",
    "tiktok.com",
    "instagram.com",
    "youtube.com",
    "reddit.com",
    // v.redd.it videos, i.redd.it images and redd.it short links.
    "redd.it",
    "linkedin.com",
    "fb.watch",
    "twitch.tv",
    "vimeo.com",
    "soundcloud.com",
    "pinterest.com",
    "pin.it",
];

/// A board whose thread links are fetched with gallery-dl (see `thread_link`):
/// its thread pages, then the domain its files are served from.
const BOARD_DOMAINS: &[&str] = &["4chan.org", "4channel.org", "4cdn.org"];

/// Mirrors people paste instead of the real link because they give nicer
/// chat previews (fxtwitter and the like). yt-dlp only knows the real sites.
const MIRRORS: &[(&str, &str)] = &[
    ("fxtwitter.com", "x.com"),
    ("vxtwitter.com", "x.com"),
    ("fixupx.com", "x.com"),
    ("fixvx.com", "x.com"),
    ("ddinstagram.com", "www.instagram.com"),
    ("kkinstagram.com", "www.instagram.com"),
    ("vxtiktok.com", "www.tiktok.com"),
    ("tnktok.com", "www.tiktok.com"),
    ("rxddit.com", "www.reddit.com"),
    ("vxreddit.com", "www.reddit.com"),
];

/// Sites with nothing but sound, which are fetched as audio in any mode.
const AUDIO_ONLY_DOMAINS: &[&str] = &["soundcloud.com"];

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
/// clings to a link in prose, turns mirror links into the real site's, and
/// rejects links that do not point at one video or post (see `is_media_link`).
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
    let mut url = url::Url::parse(&with_scheme).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if let Some((_, real)) = MIRRORS.iter().find(|(mirror, _)| host_matches(&host, mirror)) {
        url.set_host(Some(real)).ok()?;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    // Reddit's image viewer wraps the picture's own address:
    // reddit.com/media?url=https://i.redd.it/abc.jpg
    if host_matches(&host, "reddit.com") && url.path() == "/media" {
        let inner = url
            .query_pairs()
            .find(|(k, _)| k == "url")
            .and_then(|(_, v)| url::Url::parse(&v).ok());
        if let Some(inner) = inner.filter(|u| u.host_str().is_some_and(|h| host_matches(h, "redd.it"))) {
            url = inner;
        }
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let supported = SUPPORTED_DOMAINS.iter().chain(BOARD_DOMAINS).any(|d| host_matches(&host, d));
    if !supported || !is_media_link(&host, &url) {
        return None;
    }
    Some(url.into())
}

/// Path segments of a URL, without empty ones.
fn segments(url: &url::Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Whether a link to a supported site points at one video or post, rather
/// than at a profile, channel, playlist, board or live stream. Those would
/// set the downloaders walking a whole feed (a profile shared in a group
/// would be answered with twenty of its posts) or recording until the
/// timeout, so they are not treated as links to download.
fn is_media_link(host: &str, url: &url::Url) -> bool {
    let path = segments(url);
    // A marker segment followed by an id: `/status/123`, `/p/C3x...`.
    let marked = |markers: &[&str]| path.windows(2).any(|w| markers.contains(&w[0]));
    let query_has = |key: &str| url.query_pairs().any(|(k, _)| k == key);
    let is = |domain: &str| host_matches(host, domain);

    if is("youtube.com") {
        match path.as_slice() {
            ["watch", ..] => query_has("v"),
            [kind, _, ..] => ["shorts", "live", "embed", "v", "clip"].contains(kind),
            _ => false,
        }
    } else if is("x.com") || is("twitter.com") {
        marked(&["status"])
    } else if is("instagram.com") {
        matches!(path.as_slice(), ["stories", _, _, ..])
            || marked(&["p", "reel", "reels", "tv", "share"])
    } else if is("tiktok.com") {
        is("vm.tiktok.com") || is("vt.tiktok.com") || marked(&["video", "photo", "t", "embed", "v"])
    } else if is("reddit.com") {
        marked(&["comments", "s", "gallery"])
    } else if is("facebook.com") {
        match path.as_slice() {
            // watch/?v=, story.php?story_fbid=, photo.php?fbid=, media/set/?set=
            [_] | ["media", "set"] => ["v", "story_fbid", "fbid", "set"].iter().any(|k| query_has(k)),
            // reel/<id>, share/v/<id>, <page>/videos/<id>, groups/<g>/posts/<id>;
            // not profiles, groups or their listings (`<page>/videos`).
            _ => marked(&["reel", "share", "videos", "posts", "photos", "permalink"]),
        }
    } else if is("linkedin.com") {
        // posts/<slug>, feed/update/<urn>; not profiles (`in/<name>`) or companies.
        marked(&["posts", "update", "events"])
    } else if BOARD_DOMAINS.iter().any(|d| is(d)) {
        thread_link(url.as_str()).is_some()
    } else if is("twitch.tv") {
        // Clips only: a channel is a live stream and a VOD runs for hours.
        (is("clips.twitch.tv") && !path.is_empty()) || marked(&["clip"])
    } else if is("pinterest.com") {
        marked(&["pin"])
    } else if is("soundcloud.com") {
        const LISTINGS: &[&str] = &[
            "tracks", "likes", "sets", "reposts", "albums", "popular-tracks", "followers",
            "following", "comments",
        ];
        match path.as_slice() {
            _ if is("on.soundcloud.com") => !path.is_empty(),
            [_, second] => !LISTINGS.contains(second),
            [_, _, _, ..] => true,
            _ => false,
        }
    } else if is("vimeo.com") {
        path.iter().any(|s| s.bytes().all(|b| b.is_ascii_digit()))
    } else {
        // youtu.be, fb.watch, redd.it, pin.it: any path names one item.
        !path.is_empty() || url.query().is_some()
    }
}

/// What a thread link on one of BOARD_DOMAINS points at.
#[derive(Debug, PartialEq)]
pub enum ThreadLink {
    /// A post: the one the link points at (`#p123`), or the opening post,
    /// whose number is the thread's, when it points at none.
    Post(u64),
    /// One of the files posted there, linked directly.
    File,
}

/// Parse a link to a thread or to a file in one. Board and catalog pages are
/// not links to anything in particular and give `None`.
pub fn thread_link(url: &str) -> Option<ThreadLink> {
    let url = url::Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let path = segments(&url);
    if host.strip_prefix("boards.").is_some_and(|domain| BOARD_DOMAINS.contains(&domain)) {
        let [_board, "thread", thread, ..] = path.as_slice() else { return None };
        let thread: u64 = thread.parse().ok()?;
        // "#p123" is a link to a post, "#q123" the same post's quote link.
        let post = url
            .fragment()
            .and_then(|f| f.strip_prefix('p').or_else(|| f.strip_prefix('q')))
            .and_then(|n| n.parse().ok());
        return Some(ThreadLink::Post(post.unwrap_or(thread)));
    }
    if BOARD_DOMAINS.iter().any(|d| host_matches(&host, d)) {
        let [_board, file] = path.as_slice() else { return None };
        let ext = file.rsplit_once('.')?.1.to_ascii_lowercase();
        return ["webm", "mp4", "jpg", "jpeg", "png", "gif"]
            .contains(&ext.as_str())
            .then_some(ThreadLink::File);
    }
    None
}

/// Links that go to gallery-dl rather than yt-dlp first: Instagram posts,
/// which can be photos as well as videos, and thread links, which yt-dlp
/// does not know.
pub fn prefers_gallery(url: &str) -> bool {
    is_instagram_post(url) || thread_link(url).is_some()
}

/// Whether a link is to a site with nothing but sound, such as SoundCloud.
pub fn is_audio_site(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| AUDIO_ONLY_DOMAINS.iter().any(|d| host_matches(&host, d)))
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
        // yt-dlp ends up here for image posts: a Reddit picture is handed on
        // to i.redd.it, a TikTok slideshow has no extractor of its own.
        "unsupported url",
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
    } else if lower.contains("unsupported url") {
        "Са тог линка не умем ништа да преузмем.".to_string()
    } else if lower.contains("found nothing to download") {
        "У тој објави нема ни слике ни видеа.".to_string()
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
    fn links_to_one_video_or_post_are_accepted() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ&start_radio=1",
            "https://youtube.com/shorts/abc123",
            "https://m.youtube.com/watch?v=abc",
            "https://music.youtube.com/watch?v=abc",
            "https://www.youtube.com/live/abc",
            "https://youtu.be/abc?si=x",
            "https://x.com/user/status/123/photo/1",
            "https://twitter.com/i/web/status/123",
            "https://www.instagram.com/someone/reel/C3x/",
            "https://www.instagram.com/share/BAxyz",
            "https://www.instagram.com/stories/someone/3312/",
            "https://vm.tiktok.com/ZMabc/",
            "https://www.tiktok.com/@user/video/123",
            "https://www.tiktok.com/@user/photo/123",
            "https://www.tiktok.com/t/ZTabc/",
            "https://www.reddit.com/r/pics/comments/abc/title/",
            "https://www.reddit.com/r/pics/s/AbCdEf1234",
            "https://v.redd.it/abc123",
            "https://i.redd.it/abc.jpg",
            "https://redd.it/abc",
            "https://www.facebook.com/watch/?v=123",
            "https://www.facebook.com/reel/123",
            "https://www.facebook.com/share/v/abc/",
            "https://www.facebook.com/somepage/videos/123/",
            "https://m.facebook.com/story.php?story_fbid=1&id=2",
            "https://www.facebook.com/groups/somegroup/posts/123/",
            "https://www.facebook.com/photo/?fbid=123&set=a.1",
            "https://www.facebook.com/media/set/?set=a.123",
            "https://fb.watch/abc/",
            "https://www.linkedin.com/posts/someone_title-activity-123-abcd",
            "https://www.linkedin.com/feed/update/urn:li:activity:7123456789/",
            "https://clips.twitch.tv/FaintLightGullWholeWheat",
            "https://www.twitch.tv/somestreamer/clip/FaintLightGull-abc",
            "https://vimeo.com/123456",
            "https://player.vimeo.com/video/123456",
            "https://soundcloud.com/artist/track-name",
            "https://on.soundcloud.com/AbCd",
            "https://www.pinterest.com/pin/123456/",
            "https://pin.it/abc",
        ] {
            assert!(supported_url(url).is_some(), "{url} should be accepted");
        }
    }

    #[test]
    fn profiles_channels_and_live_channels_are_ignored() {
        for url in [
            "https://www.youtube.com/@channel",
            "https://www.youtube.com/channel/UC123",
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/watch",
            "https://x.com/someone",
            "https://x.com/someone/media",
            "https://www.instagram.com/someone/",
            "https://www.instagram.com/stories/someone/",
            "https://www.tiktok.com/@someone",
            "https://www.reddit.com/r/pics/",
            "https://www.reddit.com/user/someone/",
            "https://www.facebook.com/someone",
            "https://www.facebook.com/someone/photos",
            "https://www.facebook.com/profile.php?id=123",
            "https://www.facebook.com/people/Some-One/1000123/",
            "https://www.facebook.com/somepage/videos/",
            "https://www.facebook.com/groups/somegroup",
            "https://www.facebook.com/events/123456/",
            "https://www.linkedin.com/in/someone/",
            "https://www.linkedin.com/company/acme/",
            "https://www.twitch.tv/somestreamer",
            "https://www.twitch.tv/videos/123456",
            "https://soundcloud.com/artist",
            "https://soundcloud.com/artist/tracks",
            "https://vimeo.com/someuser",
            "https://www.pinterest.com/someone/board/",
        ] {
            assert_eq!(supported_url(url), None, "{url} should be ignored");
        }
    }

    #[test]
    fn mirror_links_point_at_the_real_site() {
        let real = |url: &str| supported_url(url);
        assert_eq!(real("https://fxtwitter.com/user/status/123").as_deref(), Some("https://x.com/user/status/123"));
        assert_eq!(real("vxtwitter.com/user/status/123").as_deref(), Some("https://x.com/user/status/123"));
        assert_eq!(real("https://d.fixupx.com/user/status/123").as_deref(), Some("https://x.com/user/status/123"));
        assert_eq!(
            real("https://www.ddinstagram.com/reel/C3x/").as_deref(),
            Some("https://www.instagram.com/reel/C3x/")
        );
        assert_eq!(
            real("https://vxtiktok.com/@user/video/123").as_deref(),
            Some("https://www.tiktok.com/@user/video/123")
        );
        assert_eq!(
            real("https://old.rxddit.com/r/x/comments/abc/").as_deref(),
            Some("https://www.reddit.com/r/x/comments/abc/")
        );
        // A profile behind a mirror is still a profile.
        assert_eq!(real("https://fxtwitter.com/someone"), None);
        // Reddit's image viewer link gives way to the image itself.
        assert_eq!(
            real("https://www.reddit.com/media?url=https%3A%2F%2Fi.redd.it%2Fabc123.jpeg").as_deref(),
            Some("https://i.redd.it/abc123.jpeg")
        );
        assert_eq!(real("https://www.reddit.com/media?url=https%3A%2F%2Fevil.example%2Fx.jpg"), None);
    }

    /// `https://<sub>.<BOARD_DOMAINS[domain]><path>`.
    fn board_url(sub: &str, domain: usize, path: &str) -> String {
        format!("https://{sub}.{}{path}", BOARD_DOMAINS[domain])
    }

    #[test]
    fn thread_links_name_one_post_or_file() {
        let thread = |path: &str| thread_link(&board_url("boards", 0, path));
        let post = |n| Some(ThreadLink::Post(n));
        assert_eq!(thread("/board/thread/5512345"), post(5512345));
        assert_eq!(thread("/board/thread/5512345/some-title#p5512399"), post(5512399));
        assert_eq!(thread_link(&board_url("boards", 1, "/board/thread/1#q2")), post(2));
        assert_eq!(thread("/board/thread/5512345#bottom"), post(5512345));
        assert_eq!(thread("/board/thread/abc"), None);
        // Board pages and catalogs are not one post.
        assert_eq!(thread("/board/"), None);
        assert_eq!(thread("/board/catalog"), None);
        assert_eq!(thread_link(&board_url("www", 0, "/")), None);
        // Files, on the file domain or the older host.
        assert_eq!(thread_link(&board_url("i", 2, "/board/1700000000101.webm")), Some(ThreadLink::File));
        assert_eq!(thread_link(&board_url("is2", 0, "/board/1700000000101.GIF")), Some(ThreadLink::File));
        assert_eq!(thread_link(&board_url("i", 2, "/board/1700000000101.html")), None);
        // Only those count as links to download.
        assert!(supported_url(&board_url("boards", 0, "/board/thread/1#p2")).is_some());
        assert!(supported_url(&board_url("i", 2, "/board/1.webm")).is_some());
        assert_eq!(supported_url(&board_url("boards", 0, "/board/catalog")), None);
        assert!(prefers_gallery(&board_url("boards", 0, "/board/thread/1")));
        assert!(prefers_gallery("https://www.instagram.com/p/C3x/"));
        assert!(!prefers_gallery("https://youtu.be/abc"));
    }

    #[test]
    fn audio_only_sites_are_recognised() {
        assert!(is_audio_site("https://soundcloud.com/artist/track"));
        assert!(is_audio_site("https://on.soundcloud.com/AbCd"));
        assert!(!is_audio_site("https://youtu.be/abc"));
    }

    #[test]
    fn image_posts_yt_dlp_cannot_handle_go_to_gallery_dl() {
        assert!(is_no_video("ERROR: Unsupported URL: https://i.redd.it/abc.jpg"));
        assert!(is_no_video("ERROR: [twitter] 123: No video could be found in this tweet"));
    }

    #[test]
    fn new_failures_are_explained_in_serbian() {
        assert_eq!(
            user_facing_error("gallery-dl failed (exit status: 64): [gallery-dl][error] Unsupported URL 'https://x/'"),
            "Грешка: Са тог линка не умем ништа да преузмем."
        );
        assert_eq!(
            user_facing_error("gallery-dl found nothing to download: "),
            "Грешка: У тој објави нема ни слике ни видеа."
        );
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
