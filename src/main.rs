use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use tracing::{error, info, warn};
use yt_dlp::Downloader;

mod config;
mod download;
mod handlers;
mod helpers;
mod types;

use types::{Cmd, PendingDownloads, SharedDownloader, UserModes};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tgbot=info".parse().unwrap()),
        )
        .init();

    let token = std::env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN must be set");

    let config = config::Config::from_env();

    // Initialize yt-dlp downloader (auto-downloads yt-dlp + ffmpeg binaries)
    info!("Initializing yt-dlp downloader...");
    let downloader: SharedDownloader = Arc::new(
        Downloader::with_new_binaries(PathBuf::from("libs"), PathBuf::from("output"))
            .await
            .expect("Failed to install yt-dlp/ffmpeg binaries")
            .with_timeout(config.download_timeout)
            .build()
            .await
            .expect("Failed to build downloader"),
    );
    info!("yt-dlp downloader ready");

    let bot = Bot::new(&token);

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

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![modes, pending, downloader])
        .default_handler(|upd| async move {
            warn!("Unhandled update: {:?}", upd.id);
        })
        .error_handler(LoggingErrorHandler::with_custom_text("Handler error"))
        .build()
        .dispatch()
        .await;
}
