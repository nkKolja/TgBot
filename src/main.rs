use std::sync::Arc;

use dashmap::DashMap;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use tracing::{error, warn};

mod config;
mod download;
mod handlers;
mod helpers;
mod types;

use config::Config;
use types::{Cmd, UserModes};

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

    let config = Arc::new(Config::from_env());

    let bot = Bot::new(&token);

    if let Err(e) = bot.set_my_commands(Cmd::bot_commands()).await {
        error!("Failed to set bot commands: {e}");
    }

    let modes: UserModes = Arc::new(DashMap::new());

    let handler = dptree::entry().branch(
        Update::filter_message()
            .branch(
                dptree::entry()
                    .filter_command::<Cmd>()
                    .endpoint(handlers::handle_command),
            )
            .branch(dptree::entry().endpoint(handlers::handle_message)),
    );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![modes, config])
        .default_handler(|upd| async move {
            warn!("Unhandled update: {:?}", upd.id);
        })
        .error_handler(LoggingErrorHandler::with_custom_text("Handler error"))
        .build()
        .dispatch()
        .await;
}
