# TgBot (Rust)

Telegram bot that downloads videos, audio and photos from links and sends them back in the chat.

Supported platforms: YouTube, Facebook, Instagram, TikTok, Twitter/X, Reddit, LinkedIn, and 1800+ more via yt-dlp can be enabled.

## Requirements

- **Rust** (1.80+) — [install](https://rustup.rs/)
- **yt-dlp** — `brew install yt-dlp`
- **gallery-dl** — `brew install gallery-dl` (photo posts and carousels; optional, but without it photo posts fail).
  On **macOS 12 or older** Homebrew has no ready-made packages and tries to compile
  Python, LLVM and Rust from source; install it with [uv](https://docs.astral.sh/uv/) instead, which needs no compiling:
  ```bash
  curl -LsSf https://astral.sh/uv/install.sh | sh
  ~/.local/bin/uv tool install --python 3.13 gallery-dl
  ```
  The bot finds it in `~/.local/bin` on its own, and `service update` keeps it current.
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

`yt-dlp`, `gallery-dl`, `ffmpeg`, and `ffprobe` are auto-detected from PATH and common locations (`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`). To override:

```
YTDLP_BIN=/opt/homebrew/bin/yt-dlp
GALLERY_DL_BIN=/usr/local/bin/gallery-dl
FFMPEG_BIN=/usr/local/bin/ffmpeg
FFPROBE_BIN=/usr/local/bin/ffprobe
```

### Optional: cookies for private / age-restricted posts

yt-dlp has **no username/password login** for any site this bot supports, so
the bot does not read `*_USERNAME`/`*_PASSWORD` any more (it logs a warning if
a password is still set). Private and age-restricted posts fail with
*"Age-restricted content — log in to continue"* without cookies. **Cookies are
the only way in.**

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

### Re-encoding and older hardware

Video is downloaded as H.264 whenever the site offers it, so most links go out
with no re-encode at all. Only VP9/AV1 video or Opus audio is converted, and
each stream is handled separately: a file with H.264 video and Opus audio gets
its audio converted while the video is copied. x264 runs with the `veryfast`
preset and one encode at a time, which keeps an older Xeon Mac Pro inside the
timeouts. See `X264_PRESET`, `MAX_CONCURRENT_ENCODES` and `ENCODE_TIMEOUT_SECS`.

### Photos and carousels

yt-dlp only downloads video, so photo posts go through
[gallery-dl](https://github.com/mikf/gallery-dl):

- **Instagram posts** (`instagram.com/p/…`) go to gallery-dl straight away,
  since a post can be photos, a video, or a carousel mixing both. Reels stay
  with yt-dlp.
- **Any other link** where yt-dlp reports that there is no video (an image
  tweet, say) is retried as a photo post. This also applies in `/audio` mode.

Cookies work as for yt-dlp: anonymous first, then one retry with browser
cookies. Instagram does not show posts to logged-out visitors, so in practice
Instagram posts always use your Firefox login.

What gets sent, within Telegram's limits:

- Photos and videos go out as **albums** of up to 10, in post order. A carousel
  of 20 becomes two albums. Each album is kept under the upload limit in total.
- **Photos** must be JPEG or PNG, at most 10 MB, with width + height at most
  10000. Anything else (WebP, HEIC, AVIF, oversized images) is converted to
  JPEG and scaled to at most 2560 px, the largest size Telegram displays.
- Images more elongated than 20:1 (long screenshots) are sent as **files**,
  because Telegram refuses them as photos. GIFs are sent as **animations**.
- **Videos** in a post get the same H.264 conversion and size fitting as
  ordinary video links.
- Telegram's flood control ("Too Many Requests") is waited out rather than
  treated as an error, and items are paced about a second apart.
- At most 20 items are sent per link.

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

Updates external tools (yt-dlp, gallery-dl, ffmpeg) through whatever installed
them (Homebrew, uv, or yt-dlp's own updater), rebuilds, and restarts the service.
On macOS 12 and older it skips `brew upgrade`, which would compile from source there.
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
brew install yt-dlp gallery-dl ffmpeg firefox
```

On macOS 12 or older (a 2013 Mac Pro, say), Homebrew compiles everything from
source, which can take hours and often fails. Install gallery-dl with uv as
shown under [Requirements](#requirements); yt-dlp can go the same way
(`uv tool install --python 3.13 yt-dlp`).

```bash
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
yt-dlp --cookies-from-browser firefox -F "https://www.instagram.com/reel/SOME_REEL/"
gallery-dl --cookies-from-browser firefox -g "https://www.instagram.com/p/SOME_POST/"
```

A list of formats from the first and a list of image URLs from the second mean
you are done. `could not find firefox cookies database`, or gallery-dl
reporting a redirect to the login page, means step 3 did not stick.

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
- Instagram posts and other photo posts come back as photos (albums for carousels)
- For long videos the bot asks whether to download as audio or continue as video
- The link can sit anywhere in the message ("look at this https://youtu.be/…"), or behind a text link
- Downloads are capped at `DOWNLOAD_TIMEOUT_SECS` (60s by default) in total, to avoid wasting time on files that exceed Telegram's size limit
- The audio/video buttons expire after 10 minutes and only work for the person who sent the link

In groups, the bot only responds to supported links and stays silent on errors. Make sure **Group Privacy** is turned off in BotFather (`Bot Settings → Group Privacy → Turn off`).

## Configuration

| Variable | Default | Description |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | *required* | Bot token from BotFather |
| `YTDLP_BIN` | auto-detected | Path to yt-dlp binary |
| `FFMPEG_BIN` | auto-detected | Path to ffmpeg binary |
| `FFPROBE_BIN` | auto-detected | Path to ffprobe binary |
| `GALLERY_DL_BIN` | auto-detected | Path to gallery-dl binary (photo posts) |
| `DOWNLOAD_TIMEOUT_SECS` | `60` | Max seconds for downloading one link, across all attempts. Downloads exceeding this are cancelled to keep the bot responsive. |
| `ENCODE_TIMEOUT_SECS` | `600` | Max seconds for any single ffmpeg run (convert, shrink, split). |
| `X264_PRESET` | `veryfast` | x264 preset for re-encodes. `veryfast` suits older Intel Macs; `fast` or `medium` give smaller files on faster CPUs. |
| `MAX_CONCURRENT_ENCODES` | `1` | How many ffmpeg jobs may run at once. x264 already uses every core, so raising this rarely helps. |
| `MAX_UPLOAD_MB` | `50` | Largest upload the Bot API accepts. Raise only with a self-hosted Bot API server. |
| `TELEGRAM_API_URL` | *(unset)* | Self-hosted Bot API server URL. |
| `*_COOKIES`, `COOKIES_FILE`, `COOKIES_FROM_BROWSER`, `COOKIES_AUTO` | see above | Cookie handling for login-gated posts. |
| `LONG_VIDEO_SECS` | `300` | Videos longer than this (in seconds) trigger an audio/video choice prompt |
| `RUST_LOG` | `tgbot=info` | Log level filter |
