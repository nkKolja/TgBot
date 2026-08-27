use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct ServiceCredentials {
    pub username: Option<String>,
    pub password: Option<String>,
    /// Netscape-format cookie jar exported from a logged-in browser session.
    /// yt-dlp ignores --username/--password for every site this bot supports,
    /// so cookies are the only way to reach login-gated or age-gated posts.
    pub cookies: Option<PathBuf>,
}

impl ServiceCredentials {
    fn from_env(prefix: &str) -> Self {
        Self {
            username: std::env::var(format!("{prefix}_USERNAME")).ok().filter(|s| !s.is_empty()),
            password: std::env::var(format!("{prefix}_PASSWORD")).ok().filter(|s| !s.is_empty()),
            cookies: std::env::var(format!("{prefix}_COOKIES"))
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        }
    }

    pub fn is_set(&self) -> bool {
        self.cookies.is_some() || (self.username.is_some() && self.password.is_some())
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
        let creds = if url.contains("facebook.com") {
            &self.facebook
        } else if url.contains("instagram.com") {
            &self.instagram
        } else if url.contains("x.com") || url.contains("twitter.com") {
            &self.twitter
        } else if url.contains("tiktok.com") {
            &self.tiktok
        } else if url.contains("reddit.com") {
            &self.reddit
        } else if url.contains("linkedin.com") {
            &self.linkedin
        } else if url.contains("youtube.com") || url.contains("youtu.be") {
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
    pub download_timeout: Duration,
    pub long_video_secs: f64,
    /// Largest file the Bot API will accept from us. 50 MB on api.telegram.org,
    /// up to 2000 MB against a self-hosted Bot API server.
    pub max_upload_bytes: u64,
    /// Base URL of a self-hosted Bot API server, if any.
    pub telegram_api_url: Option<String>,
    /// Cookie jar used for any site without a service-specific one.
    pub cookies_file: Option<PathBuf>,
    /// `--cookies-from-browser` spec (e.g. `chrome`, `firefox:default`).
    pub cookies_from_browser: Option<String>,
    /// Retry a login-walled download once with cookies from a detected browser.
    pub cookies_auto: bool,
    pub services: Services,
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
            download_timeout: Duration::from_secs(timeout_secs),
            long_video_secs,
            max_upload_bytes: max_upload_mb * 1024 * 1024,
            telegram_api_url: std::env::var("TELEGRAM_API_URL").ok().filter(|s| !s.is_empty()),
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
        }
    }
}

fn which(name: &str) -> PathBuf {
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
    ];
    for dir in candidates {
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return p;
        }
    }

    PathBuf::from(name)
}
