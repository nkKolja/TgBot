use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use teloxide::prelude::*;
use teloxide::update_listeners;
use teloxide::utils::command::BotCommands;
use tracing::{debug, error, info, warn};

mod config;
mod download;
mod gallery;
mod handlers;
mod helpers;
mod service;
mod types;

use config::Config;
use types::{Cmd, PendingDownloads, UserModes};

#[tokio::main]
async fn main() {
    // Handle `tgbot service <subcommand>` before starting the bot.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("service") {
        let code = service::run(&args[2..]);
        std::process::exit(if code == std::process::ExitCode::SUCCESS { 0 } else { 1 });
    }

    dotenvy::dotenv().ok();

    // RUST_LOG replaces the default outright. Adding "tgbot=info" on top of it
    // used to override whatever level RUST_LOG gave tgbot, so debug logging
    // could not be turned on.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("tgbot=info")),
        )
        .init();

    let token = std::env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN must be set");

    let config = Arc::new(Config::from_env());
    info!(
        ytdlp = %config.ytdlp_bin.display(),
        ffmpeg = %config.ffmpeg_bin.display(),
        ffprobe = %config.ffprobe_bin.display(),
        gallery_dl = %config.gallery_dl_bin.display(),
        "Resolved tool paths"
    );
    if !config.gallery_dl_bin.is_file() {
        warn!("gallery-dl not found: photo posts will not work (see README, Requirements)");
    }

    // teloxide's default HTTP client gives up on any request after 17 seconds,
    // uploads included, so a 50 MB video on an ordinary uplink never finished.
    // Handlers get a client that waits long enough for the largest upload;
    // long polling keeps the default, so a dropped connection is still noticed
    // within seconds.
    let client = teloxide::net::default_reqwest_settings()
        .timeout(request_timeout(config.max_upload_bytes))
        .build()
        .expect("could not create the HTTP client");
    let mut bot = Bot::with_client(&token, client);
    let mut polling_bot = Bot::new(&token);
    // A self-hosted Bot API server lifts the upload cap from 50 MB to 2000 MB.
    if let Some(api_url) = &config.telegram_api_url {
        match url::Url::parse(api_url) {
            Ok(u) => {
                info!("Using local Bot API server at {u}");
                bot = bot.set_api_url(u.clone());
                polling_bot = polling_bot.set_api_url(u);
            }
            Err(e) => error!("Invalid TELEGRAM_API_URL {api_url}: {e}"),
        }
    }

    if let Err(e) = bot.set_my_commands(Cmd::bot_commands()).await {
        error!("Failed to set bot commands: {e}");
    }

    let modes: UserModes = Arc::new(DashMap::new());
    let pending: PendingDownloads = Arc::new(DashMap::new());

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .branch(
                    dptree::entry()
                        .filter_command::<Cmd>()
                        .endpoint(handlers::handle_command),
                )
                .branch(dptree::entry().endpoint(handlers::handle_message)),
        )
        .branch(
            Update::filter_callback_query()
                .endpoint(handlers::handle_callback),
        );

    let listener = update_listeners::polling_default(polling_bot).await;
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![modes, pending, config])
        // Edits, joins and the like: with Group Privacy off there is one for
        // every edited message in every group.
        .default_handler(|upd| async move {
            debug!("Unhandled update: {:?}", upd.id);
        })
        .error_handler(LoggingErrorHandler::with_custom_text("Handler error"))
        .build()
        .dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("An error from the update listener"),
        )
        .await;
}

/// Time allowed for one Bot API request: long enough to upload the largest
/// file at about 1.3 Mbit/s, and never less than five minutes.
fn request_timeout(max_upload_bytes: u64) -> Duration {
    let mb = max_upload_bytes / (1024 * 1024);
    Duration::from_secs((mb * 6).max(300))
}

/// Held by tests that write a script and run it. Run in parallel, one test's
/// child process, forked while another test's script is still open for
/// writing, makes running that script fail with "Text file busy".
#[cfg(test)]
pub(crate) fn exec_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
