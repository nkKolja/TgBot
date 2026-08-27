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

### Optional: cookies for private / age-restricted posts

yt-dlp has **no username/password login** for any site this bot supports. Setting
`INSTAGRAM_USERNAME`/`INSTAGRAM_PASSWORD` (or the equivalent for Facebook, X,
TikTok, Reddit, LinkedIn, YouTube) does nothing except print

```
WARNING: Login with password is not supported for this website.
```

so private and age-restricted posts still fail with *"Age-restricted content —
log in to continue"*. **Cookies are the only way in.**

Export a Netscape-format `cookies.txt` while logged in to the site (browser
extensions such as "Get cookies.txt LOCALLY" do this) and point at it:

```
INSTAGRAM_COOKIES=/Users/you/.config/tgbot/instagram-cookies.txt
```

Same pattern for `FACEBOOK_`, `TWITTER_`, `TIKTOK_`, `REDDIT_`, `LINKEDIN_`,
`YOUTUBE_`. A shared jar for every other site:

```
COOKIES_FILE=/Users/you/.config/tgbot/cookies.txt
```

Or read them straight from a browser profile:

```
COOKIES_FROM_BROWSER=firefox
```

**This happens automatically.** With no cookies configured at all, a download
that hits a login or age wall is retried once with `--cookies-from-browser`
against the first browser profile found on disk, in this order: Firefox,
Chrome, Brave, Edge, Vivaldi, Chromium. Nothing to configure — the browser just
has to be logged in to the site. Turn it off with `COOKIES_AUTO=false`.

Firefox is tried first on purpose. Its cookie jar is plain SQLite in your home
directory, so it works from a launchd background service. Chromium browsers
encrypt theirs with a macOS Keychain entry ("Chrome Safe Storage") that a
background daemon usually cannot unlock, and Safari needs Full Disk Access — if
the bot runs as a service, log in to the sites with Firefox.

There is no automated *login*: yt-dlp cannot sign in on your behalf for any of
these sites, so some browser on the machine must already hold a live session.

Keep cookie files out of the repo and `chmod 600` them; they are live session
credentials.

### File size limits

The Telegram Bot API caps bot uploads at **50 MB for every type** — audio,
video, and document alike, so sending a large file "as a document" does not get
around it. The bot instead makes the file fit:

- **Audio** over the limit is re-encoded to whatever AAC bitrate fits (down to
  32 kbps mono). If it is still too long, it is split into numbered parts and
  sent one after another.
- **Video** over the limit is re-encoded at a computed bitrate and scaled down
  to 720p or 480p. If it still will not fit, the bot suggests `/audio`.
- Anything Telegram refuses to treat as playable media falls back to being sent
  as a plain document.

To lift the cap properly, run your own
[Bot API server](https://github.com/tdlib/telegram-bot-api) (2000 MB) and set:

```
TELEGRAM_API_URL=http://localhost:8081
MAX_UPLOAD_MB=2000
```

### Auto-dubbed audio

YouTube serves AI auto-dubbed audio tracks as the regional default, so
downloads could come back dubbed instead of in the creator's own voice. The bot
now selects on yt-dlp's `language_preference>=10`, which is how the *original*
track is marked, and falls back to the normal best-audio choice for videos with
no dubs.

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
