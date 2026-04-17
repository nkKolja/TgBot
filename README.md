# TgBot (Rust)

Telegram bot that downloads videos and audio from links and sends them back in the chat.

Supported platforms: YouTube, Facebook, Instagram, TikTok, Twitter/X, Reddit, LinkedIn, and 1800+ more via yt-dlp can be enabled.

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

`yt-dlp`, `ffmpeg`, and `ffprobe` are auto-detected from PATH and common locations (`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`). To override:

```
YTDLP_BIN=/opt/homebrew/bin/yt-dlp
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

Service management is built into the binary. Run commands with `cargo run --`:

### Install (first time)

Builds a release binary, registers the launchd service, and starts the bot:

```bash
cargo run -- service install
```

### Update (after pulling changes)

Updates external tools (yt-dlp, ffmpeg), rebuilds, and restarts the service.
Also works without the service installed — just updates tools and rebuilds:

```bash
cargo run -- service update
```

### Uninstall

Stops the service and removes the plist:

```bash
cargo run -- service uninstall
```

### Status

```bash
cargo run -- service status
```

The service auto-starts on login and auto-restarts on crash (with a 10s cooldown). Logs are written to `tgbot.log` and `tgbot-error.log` in the repo directory.

## Bot usage

- `/start` — start the bot
- `/video` — switch to video mode (default)
- `/audio` — switch to audio mode
- Send a link — bot downloads and sends back the video or audio
- For long videos the bot asks whether to download as audio or continue as video
- Downloads are capped at 30s to avoid wasting time on files that exceed Telegram's size limit

In groups, the bot only responds to supported links and stays silent on errors. Make sure **Group Privacy** is turned off in BotFather (`Bot Settings → Group Privacy → Turn off`).

## Configuration

| Variable | Default | Description |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | *required* | Bot token from BotFather |
| `YTDLP_BIN` | auto-detected | Path to yt-dlp binary |
| `FFMPEG_BIN` | auto-detected | Path to ffmpeg binary |
| `FFPROBE_BIN` | auto-detected | Path to ffprobe binary |
| `DOWNLOAD_TIMEOUT_SECS` | `30` | Max seconds for a single download. Downloads exceeding this are cancelled to keep the bot responsive. |
| `LONG_VIDEO_SECS` | `300` | Videos longer than this (in seconds) trigger an audio/video choice prompt |
| `RUST_LOG` | `tgbot=info` | Log level filter |
