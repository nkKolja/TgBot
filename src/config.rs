use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct ServiceCredentials {
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ServiceCredentials {
    fn from_env(prefix: &str) -> Self {
        Self {
            username: std::env::var(format!("{prefix}_USERNAME")).ok().filter(|s| !s.is_empty()),
            password: std::env::var(format!("{prefix}_PASSWORD")).ok().filter(|s| !s.is_empty()),
        }
    }

    pub fn is_set(&self) -> bool {
        self.username.is_some() && self.password.is_some()
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
        } else {
            return None;
        };
        if creds.is_set() { Some(creds) } else { None }
    }
}

pub struct Config {
    pub ffmpeg_bin: PathBuf,
    pub ffprobe_bin: PathBuf,
    pub download_timeout: Duration,
    pub services: Services,
}

impl Config {
    pub fn from_env() -> Self {
        let timeout_secs = std::env::var("DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        Self {
            ffmpeg_bin: std::env::var("FFMPEG_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| which("ffmpeg")),
            ffprobe_bin: std::env::var("FFPROBE_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| which("ffprobe")),
            download_timeout: Duration::from_secs(timeout_secs),
            services: Services::from_env(),
        }
    }
}

fn which(name: &str) -> PathBuf {
    std::process::Command::new("which")
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
        .unwrap_or_else(|| PathBuf::from(name))
}
