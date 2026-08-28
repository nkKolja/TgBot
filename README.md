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

Note the difference between the two: anything set above (`*_COOKIES`,
`COOKIES_FILE`, `COOKIES_FROM_BROWSER`) is sent on **every** request.

**With none of them set, cookies are used only as a fallback** — the download is
attempted anonymously, and only if it hits a login or age wall is it retried
once with `--cookies-from-browser` against the first browser profile found on
disk, in this order: Firefox, Chrome, Brave, Edge, Vivaldi, Chromium. This is
the default, it needs no configuration, and it keeps ordinary public downloads
unattached to your account. Turn it off with `COOKIES_AUTO=false`.

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

## Server setup (dedicated Mac)

Start to finish on a machine that will run the bot full time. Do all of it
logged in as the user the bot will run as.

**1. Install the tools**

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
brew install yt-dlp ffmpeg firefox
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**2. Get the bot**

```bash
git clone <this repo> ~/TgBot && cd ~/TgBot
cp .env.example .env
```

Edit `.env` and set `TELEGRAM_BOT_TOKEN`.

**3. Log in to Instagram, once**

Open Firefox and log in to Instagram normally. Then:

- Check **Settings → Privacy & Security** that *"Delete cookies and site data
  when Firefox is closed"* is **off**, and that Firefox is not set to always use
  Private Browsing. Either one throws the session away.
- **Quit Firefox** (⌘Q). This matters: yt-dlp reads `cookies.sqlite` but not its
  `-wal` sidecar, so a login that is still buffered in the WAL is invisible to
  it. Quitting flushes it.

Firefox never has to run again. Log in to YouTube in the same browser too if you
want age-restricted videos to work.

**4. Check the cookies work before involving the bot**

```bash
yt-dlp --cookies-from-browser firefox -F "https://www.instagram.com/p/SOME_POST/"
```

A list of formats means you are done. `could not find firefox cookies database`
means step 3 did not stick.

**5. Leave `.env` alone**

Nothing to configure. With `COOKIES_FROM_BROWSER` **unset**, the bot downloads
anonymously and only reaches for browser cookies after a download actually hits
a login or age wall. That is the default and it is the mode you want.

Setting `COOKIES_FROM_BROWSER=firefox` switches to sending your cookies on
*every* request, which ties ordinary public downloads to your account for no
benefit. Only do that if anonymous downloads stop working entirely.

**6. Install the service**

```bash
cargo run -- service install
cargo run -- service status
tail -f tgbot.log
```

**7. Make it survive reboots**

The bot installs as a **LaunchAgent**, which only starts inside a logged-in user
session. On a headless server, turn on **System Settings → Users & Groups →
Automatic login** for this user, or the bot stays down after a reboot until
someone logs in physically.

Also worth setting, in **System Settings → Energy**: *Prevent automatic
sleeping*, and *Start up automatically after a power failure*.

### When Instagram logs you out

Sessions last months, not forever. When downloads start failing again: open
Firefox, log in, quit Firefox. That is the whole fix — no rebuild, no restart,
the bot picks up the new cookies on its next download.

Use a throwaway Instagram account if you can. Pulling media through a session
automatically is the kind of traffic Meta flags, and a disabled account is
easier to shrug off when it is not your real one.

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
