use std::path::PathBuf;
use std::time::Duration;

use tracing::warn;

use crate::helpers::host_matches;

#[derive(Clone, Debug, Default)]
pub struct ServiceCredentials {
    /// Netscape-format cookie jar exported from a logged-in browser session.
    /// yt-dlp has no password login for any site this bot supports, so cookies
    /// are the only way to reach login-gated or age-gated posts.
    pub cookies: Option<PathBuf>,
}

impl ServiceCredentials {
    fn from_env(prefix: &str) -> Self {
        // *_USERNAME / *_PASSWORD used to be passed to yt-dlp, which ignored
        // them and printed a warning — while exposing the password to anyone
        // running `ps`. They are no longer read.
        if std::env::var(format!("{prefix}_PASSWORD")).is_ok_and(|s| !s.is_empty()) {
            warn!("{prefix}_PASSWORD is set but ignored: yt-dlp cannot log in with a password. Use {prefix}_COOKIES.");
        }
        Self {
            cookies: std::env::var(format!("{prefix}_COOKIES"))
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        }
    }

    pub fn is_set(&self) -> bool {
        self.cookies.is_some()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Services {
    pub facebook: ServiceCredentials,
    pub instagram: ServiceCredentials,
    pub twitter: ServiceCredentials,
    pub tiktok: ServiceCredentials,
    pub reddit: ServiceCredentials,
    pub linkedin: ServiceCredentials,
    pub youtube: ServiceCredentials,
}

impl Services {
    fn from_env() -> Self {
        Self {
            facebook: ServiceCredentials::from_env("FACEBOOK"),
            instagram: ServiceCredentials::from_env("INSTAGRAM"),
            twitter: ServiceCredentials::from_env("TWITTER"),
            tiktok: ServiceCredentials::from_env("TIKTOK"),
            reddit: ServiceCredentials::from_env("REDDIT"),
            linkedin: ServiceCredentials::from_env("LINKEDIN"),
            youtube: ServiceCredentials::from_env("YOUTUBE"),
        }
    }

    pub fn for_url(&self, url: &str) -> Option<&ServiceCredentials> {
        let host = url::Url::parse(url).ok()?.host_str()?.to_owned();
        let is = |domain: &str| host_matches(&host, domain);
        let creds = if is("facebook.com") || is("fb.watch") {
            &self.facebook
        } else if is("instagram.com") {
            &self.instagram
        } else if is("x.com") || is("twitter.com") {
            &self.twitter
        } else if is("tiktok.com") {
            &self.tiktok
        } else if is("reddit.com") {
            &self.reddit
        } else if is("linkedin.com") {
            &self.linkedin
        } else if is("youtube.com") || is("youtu.be") {
            &self.youtube
        } else {
            return None;
        };
        if creds.is_set() { Some(creds) } else { None }
    }
}

pub struct Config {
    pub ytdlp_bin: PathBuf,
    pub ffmpeg_bin: PathBuf,
    pub ffprobe_bin: PathBuf,
    /// gallery-dl, for photo posts and carousels. Optional: without it photo
    /// posts fail as before.
    pub gallery_dl_bin: PathBuf,
    pub download_timeout: Duration,
    pub long_video_secs: f64,
    /// Largest file the Bot API will accept from us. 50 MB on api.telegram.org,
    /// up to 2000 MB against a self-hosted Bot API server.
    pub max_upload_bytes: u64,
    /// Largest file the Bot API lets us download (videos sent to be
    /// converted): 20 MB on api.telegram.org, no limit on a self-hosted
    /// server in --local mode.
    pub max_download_bytes: u64,
    /// Base URL of a self-hosted Bot API server, if any.
    pub telegram_api_url: Option<String>,
    /// Cookie jar used for any site without a service-specific one.
    pub cookies_file: Option<PathBuf>,
    /// `--cookies-from-browser` spec (e.g. `chrome`, `firefox:default`).
    pub cookies_from_browser: Option<String>,
    /// Retry a login-walled download once with cookies from a detected browser.
    pub cookies_auto: bool,
    pub services: Services,
    /// Upper bound on any single ffmpeg run (re-encode, shrink, split).
    pub encode_timeout: Duration,
    /// x264 speed/size trade-off. `veryfast` keeps re-encodes tolerable on an
    /// old Xeon; `fast`/`medium` give smaller files on newer machines.
    pub x264_preset: String,
    /// Limits how many ffmpeg processes run at once. x264 already uses every
    /// core, so parallel encodes only make each one slower and risk timeouts.
    pub encode_slots: tokio::sync::Semaphore,
}

/// What api.telegram.org lets a bot download, in MB.
pub const BOT_API_DOWNLOAD_MB: u64 = 20;

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|s| s.trim().parse::<T>().ok())
}

impl Config {
    pub fn from_env() -> Self {
        let timeout_secs = std::env::var("DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(60);
        let long_video_secs = std::env::var("LONG_VIDEO_SECS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(300.0);
        let max_upload_mb = std::env::var("MAX_UPLOAD_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|mb| *mb > 0)
            .unwrap_or(50);
        let telegram_api_url = std::env::var("TELEGRAM_API_URL").ok().filter(|s| !s.is_empty());
        // Only a server the bot will actually use lifts the cap (main.rs falls
        // back to api.telegram.org on a URL it cannot parse). Whether it runs
        // with --local cannot be told from here; if not, Telegram's refusal
        // is turned into the same message as the 20 MB check.
        let own_server = telegram_api_url.as_deref().is_some_and(|u| url::Url::parse(u).is_ok());
        let max_download_mb = if own_server { 2000 } else { BOT_API_DOWNLOAD_MB };
        Self {
            ytdlp_bin: std::env::var("YTDLP_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| which("yt-dlp")),
            ffmpeg_bin: std::env::var("FFMPEG_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| which("ffmpeg")),
            ffprobe_bin: std::env::var("FFPROBE_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| which("ffprobe")),
            gallery_dl_bin: std::env::var("GALLERY_DL_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| which("gallery-dl")),
            download_timeout: Duration::from_secs(timeout_secs),
            long_video_secs,
            max_upload_bytes: max_upload_mb * 1024 * 1024,
            max_download_bytes: max_download_mb * 1024 * 1024,
            telegram_api_url,
            cookies_file: std::env::var("COOKIES_FILE")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            cookies_from_browser: std::env::var("COOKIES_FROM_BROWSER")
                .ok()
                .filter(|s| !s.is_empty()),
            cookies_auto: std::env::var("COOKIES_AUTO")
                .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
                .unwrap_or(true),
            services: Services::from_env(),
            encode_timeout: Duration::from_secs(
                env_parse::<u64>("ENCODE_TIMEOUT_SECS").filter(|s| *s > 0).unwrap_or(600),
            ),
            x264_preset: std::env::var("X264_PRESET")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "veryfast".to_string()),
            encode_slots: tokio::sync::Semaphore::new(
                env_parse::<usize>("MAX_CONCURRENT_ENCODES").filter(|n| *n > 0).unwrap_or(1),
            ),
        }
    }
}

/// PATH for yt-dlp and gallery-dl, when it needs extending. yt-dlp needs Deno
/// for YouTube and looks for it on PATH, but the Deno installer puts it in
/// ~/.deno/bin, which a launchd service's PATH does not include.
pub(crate) fn tool_path() -> Option<&'static std::ffi::OsStr> {
    static PATH: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let deno_dir = dirs::home_dir()?.join(".deno/bin");
        if !deno_dir.is_dir() {
            return None;
        }
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&current).collect();
        if dirs.contains(&deno_dir) {
            return None;
        }
        dirs.push(deno_dir);
        std::env::join_paths(dirs).ok()
    })
    .as_deref()
}

pub(crate) fn which(name: &str) -> PathBuf {
    // Try the system `which` first (works when PATH is correct).
    if let Some(p) = std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(PathBuf::from(
                    String::from_utf8_lossy(&o.stdout).trim().to_string(),
                ))
            } else {
                None
            }
        })
    {
        return p;
    }

    // Fallback: probe common directories (launchd has a minimal PATH).
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates: &[&str] = &[
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        &format!("{home}/.local/bin"),
        &format!("{home}/.deno/bin"),
    ];
    for dir in candidates {
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return p;
        }
    }

    PathBuf::from(name)
}
