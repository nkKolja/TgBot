use std::time::Duration;

pub struct Config {
    pub download_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let timeout_secs = std::env::var("DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(120);
        Self {
            download_timeout: Duration::from_secs(timeout_secs),
        }
    }
}
