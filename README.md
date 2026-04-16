# TgBot (Rust)

Telegram bot that downloads videos and audio from links and sends them back in the chat.

Supported platforms: YouTube, Facebook, Instagram, TikTok, Twitter/X, Reddit, LinkedIn.

## Requirements

- **Rust** (1.70+) — [install](https://rustup.rs/)
- **yt-dlp** — `brew install yt-dlp`
- **ffmpeg** (includes ffprobe) — `brew install ffmpeg`
- **Telegram bot token** — get one from [@BotFather](https://t.me/BotFather)

## Setup

```bash
cp .env.example .env
```

Edit `.env` and set your bot token:

```
TELEGRAM_BOT_TOKEN=your_token_here
```

That's it. `ffmpeg` and `ffprobe` are auto-detected from your PATH. If you need to override:

```
FFMPEG_BIN=/usr/local/bin/ffmpeg
FFPROBE_BIN=/usr/local/bin/ffprobe
```

### Optional: service credentials

For platforms that require login (e.g. private Facebook videos):

```
FACEBOOK_USERNAME=your_email
FACEBOOK_PASSWORD=your_password
```

Same pattern for `INSTAGRAM_`, `TWITTER_`, `TIKTOK_`, `REDDIT_`, `LINKEDIN_`.

## Run

```bash
cargo run
```

## Run as background service (macOS)

### Install (first time)

Builds the binary, registers the launchd service, and starts the bot:

```bash
./install-service.sh install
```

### Update (after pulling changes)

Rebuilds and restarts the running service in one step:

```bash
./install-service.sh update
```

### Uninstall

Stops the service and removes the plist:

```bash
./install-service.sh uninstall
```

### Status

```bash
./install-service.sh status
```

The service auto-starts on login and auto-restarts on crash (with a 10s cooldown). Logs are written to `tgbot.log` and `tgbot-error.log` in the repo directory.

## Bot usage

- `/start` — start the bot
- `/video` — switch to video mode (default)
- `/audio` — switch to audio mode
- Send a link — bot downloads and sends back the video or audio

In groups, the bot only responds to supported links and stays silent on errors. Make sure **Group Privacy** is turned off in BotFather (`Bot Settings → Group Privacy → Turn off`).

## Configuration

| Variable | Default | Description |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | *required* | Bot token from BotFather |
| `FFMPEG_BIN` | auto-detected | Path to ffmpeg binary |
| `FFPROBE_BIN` | auto-detected | Path to ffprobe binary |
| `DOWNLOAD_TIMEOUT_SECS` | `30` | Max seconds for a yt-dlp download |
| `RUST_LOG` | `tgbot=info` | Log level filter |
