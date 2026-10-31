//! Photo posts and carousels — Instagram posts above all — fetched with
//! gallery-dl and sent in a form Telegram displays inline.
//!
//! Telegram limits handled here:
//! - sendPhoto: at most 10 MB, width + height at most 10000, aspect ratio at
//!   most 20:1. Photos are shown at up to 2560 px on the long edge.
//! - sendPhoto shows JPEG and PNG reliably; WebP, HEIC, AVIF etc. are converted.
//! - sendMediaGroup: 2–10 photos/videos per album. GIFs (animations) and
//!   documents cannot go in a photo album and are sent on their own.
//! - Every upload: MAX_UPLOAD_MB (50 MB on api.telegram.org).
//! - Flood control: handled by `with_retry` plus a pause between batches.

use std::path::{Path, PathBuf};
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, ReplyParameters};
use teloxide::RequestError;
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;
use tracing::{info, warn};

use crate::config::Config;
use crate::download::{
    cookie_retry_browser, file_size, fit_video, named_for_sending, refused, run_ffmpeg, run_tool,
    send_document_file, send_or_document, send_video_file, telegram_friendly_video,
    upload_budget, with_retry, BoxError, Deadline,
};
use crate::helpers::{
    is_gallery_auth_failure, is_instagram_post, media_duration, video_dimensions, ytdlp_auth_args,
};
use crate::types::Target;

const PHOTO_MAX_BYTES: u64 = 10 * 1024 * 1024;
const PHOTO_MAX_DIMENSIONS: u32 = 10_000;
const PHOTO_MAX_RATIO: f64 = 20.0;
/// Telegram displays photos at most this large, so a photo that has to be
/// re-encoded anyway is scaled down to it.
const PHOTO_EDGE: u32 = 2560;
const ALBUM_MAX: usize = 10;
/// Instagram allows 20 items per post. More than that is not a post but a
/// whole gallery, which would flood the chat.
const MAX_ITEMS: usize = 20;

pub enum GalleryFailure {
    /// gallery-dl is not installed.
    Unavailable,
    Failed(BoxError),
}

impl<E: Into<BoxError>> From<E> for GalleryFailure {
    fn from(e: E) -> Self {
        Self::Failed(e.into())
    }
}

/// Download every photo and video in the post at `url` and send them to the chat.
pub async fn send_gallery(
    bot: &Bot,
    target: &Target,
    url: &str,
    config: &Config,
    deadline: Deadline,
) -> Result<(), GalleryFailure> {
    if !config.gallery_dl_bin.is_file() {
        return Err(GalleryFailure::Unavailable);
    }
    let tmp = TempDir::new()?;
    let download_dir = tmp.path().join("download");
    std::fs::create_dir_all(&download_dir)?;

    let mut files = download(url, &download_dir, config, deadline).await?;
    // --range already stops gallery-dl at this many; this is a safety net.
    if files.len() > MAX_ITEMS {
        warn!("{url} has {} files, sending the first {MAX_ITEMS}", files.len());
        files.truncate(MAX_ITEMS);
    }
    info!("gallery-dl fetched {} file(s) from {url}", files.len());

    let mut items = Vec::with_capacity(files.len());
    for (i, file) in files.iter().enumerate() {
        let work = tmp.path().join(format!("item{i}"));
        std::fs::create_dir_all(&work)?;
        match prepare(file, &work, config).await {
            Ok(item) => items.push(item),
            Err(e) => warn!("Skipping {}: {e}", file.display()),
        }
    }
    if items.is_empty() {
        return Err("none of the files in the post can be sent".into());
    }

    deliver(bot, target, &items, config).await?;
    info!("Sent {} item(s) from {url}", items.len());
    Ok(())
}

// --- Downloading ---------------------------------------------------------------

/// Fetch the post into `dir`, in post order. Like yt-dlp, gallery-dl goes out
/// without cookies first and retries once with browser cookies after a login
/// wall.
async fn download(
    url: &str,
    dir: &Path,
    config: &Config,
    deadline: Deadline,
) -> Result<Vec<PathBuf>, BoxError> {
    let dir_arg = dir.to_string_lossy().into_owned();
    let range = format!("1-{MAX_ITEMS}");
    let args = [
        "-D", dir_arg.as_str(),
        // One line per downloaded file on stdout, in order.
        "-o", "output.mode=pipe",
        // Instagram's ready-made MP4 (H.264 + AAC) instead of DASH streams,
        // which gallery-dl would pass to the yt-dlp Python module to merge.
        "-o", "extractor.instagram.videos=merged",
        // Stop once there is as much as will be sent, rather than fetching a
        // long thread or gallery in full only to throw most of it away.
        "--range", range.as_str(),
    ];
    // gallery-dl takes the same --cookies / --cookies-from-browser flags.
    let auth = ytdlp_auth_args(url, config);

    let first = attempt(&args, &auth, url, dir, config, deadline).await;
    let Err(e) = first else { return first };
    // Instagram's post API answers nobody who is logged out, whatever error
    // text that produces, so any failure there counts as a login wall.
    let wall = !e.to_string().contains("cancelled")
        && (is_gallery_auth_failure(&e.to_string()) || is_instagram_post(url));
    let Some(browser) = cookie_retry_browser(&auth, config, wall, url) else {
        return Err(e);
    };
    let mut retry = vec!["--cookies-from-browser".to_string(), browser.to_string()];
    retry.extend(auth);
    attempt(&args, &retry, url, dir, config, deadline)
        .await
        .map_err(|retry_err| {
            warn!("Cookie retry via {browser} also failed: {retry_err}");
            if retry_err.to_string().contains("cancelled") { retry_err } else { e }
        })
}

async fn attempt(
    args: &[&str],
    extra: &[String],
    url: &str,
    dir: &Path,
    config: &Config,
    deadline: Deadline,
) -> Result<Vec<PathBuf>, BoxError> {
    let out = run_tool("gallery-dl", &config.gallery_dl_bin, args, extra, url, deadline).await?;
    let files = downloaded_files(&out.stdout, dir);
    if !files.is_empty() {
        if !out.success {
            warn!(
                "gallery-dl reported errors but fetched {} file(s): {}",
                files.len(),
                out.failure_detail()
            );
        }
        return Ok(files);
    }
    if out.success {
        return Err(format!("gallery-dl found nothing to download: {}", out.failure_detail()).into());
    }
    Err(format!("gallery-dl failed ({}): {}", out.status, out.failure_detail()).into())
}

/// Files gallery-dl reports on stdout, in order. Lines starting with "# " are
/// files that were already there, which in a fresh directory only happens for
/// duplicates.
fn downloaded_files(stdout: &str, dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for line in stdout.lines() {
        let path = PathBuf::from(line.strip_prefix("# ").unwrap_or(line).trim_end());
        if path.starts_with(dir) && path.is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

// --- Preparing -----------------------------------------------------------------

/// What a downloaded file is sent as.
#[derive(Debug)]
enum Item {
    Photo(PathBuf),
    Video {
        path: PathBuf,
        width: Option<u32>,
        height: Option<u32>,
        duration: Option<f64>,
    },
    /// GIFs, which Telegram plays as animations.
    Animation(PathBuf),
    /// Anything Telegram would not display inline, delivered as a file.
    Document(PathBuf),
}

impl Item {
    fn path(&self) -> &Path {
        match self {
            Item::Photo(p) | Item::Animation(p) | Item::Document(p) => p,
            Item::Video { path, .. } => path,
        }
    }

    fn fits_album(&self) -> bool {
        matches!(self, Item::Photo(_) | Item::Video { .. })
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

async fn prepare(file: &Path, work: &Path, config: &Config) -> Result<Item, BoxError> {
    let item = match extension(file).as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "heic" | "heif" | "avif" | "bmp" | "tif" | "tiff" => {
            prepare_photo(file, work, config).await
        }
        "gif" => Item::Animation(file.to_path_buf()),
        "mp4" | "m4v" | "mov" | "webm" | "mkv" => {
            let ready = telegram_friendly_video(file, work, config).await?;
            let fitted = fit_video(&ready, work, config).await?;
            // Sent under the name it was posted with.
            let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let path = named_for_sending(&fitted, work, &stem);
            let (width, height) = video_dimensions(&config.ffprobe_bin, &path).await;
            let duration = media_duration(&config.ffprobe_bin, &path).await;
            Item::Video { path, width, height, duration }
        }
        _ => Item::Document(file.to_path_buf()),
    };
    let size = file_size(item.path());
    if size > upload_budget(config) {
        return Err(format!("{} MB is over the upload limit", size / (1024 * 1024)).into());
    }
    Ok(item)
}

#[derive(Debug, Default)]
struct ImageInfo {
    codec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

async fn probe_image(ffprobe: &Path, path: &Path) -> ImageInfo {
    let Ok(out) = AsyncCommand::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height", "-of", "default=nw=1"])
        .arg(path)
        .output()
        .await
    else {
        return ImageInfo::default();
    };
    let mut info = ImageInfo::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        match line.split_once('=') {
            Some(("codec_name", v)) => info.codec = Some(v.trim().to_ascii_lowercase()),
            Some(("width", v)) => info.width = v.trim().parse().ok(),
            Some(("height", v)) => info.height = v.trim().parse().ok(),
            _ => {}
        }
    }
    info
}

/// Long edge over short edge.
fn aspect_ratio(width: u32, height: u32) -> f64 {
    width.max(height) as f64 / width.min(height).max(1) as f64
}

/// A JPEG or PNG that sendPhoto takes as it is.
fn photo_ok_as_is(info: &ImageInfo, size: u64) -> bool {
    matches!(info.codec.as_deref(), Some("mjpeg") | Some("png"))
        && size <= PHOTO_MAX_BYTES
        && matches!((info.width, info.height), (Some(w), Some(h)) if w + h <= PHOTO_MAX_DIMENSIONS)
}

async fn prepare_photo(file: &Path, work: &Path, config: &Config) -> Item {
    let info = probe_image(&config.ffprobe_bin, file).await;
    if let (Some(w), Some(h)) = (info.width, info.height) {
        if aspect_ratio(w, h) > PHOTO_MAX_RATIO {
            // Telegram refuses photos this elongated (long screenshots, wide
            // panoramas); as a file it arrives intact.
            return Item::Document(file.to_path_buf());
        }
    }
    if photo_ok_as_is(&info, file_size(file)) {
        return Item::Photo(file.to_path_buf());
    }
    match to_jpeg(file, work, config).await {
        Ok(jpeg) if file_size(&jpeg) <= PHOTO_MAX_BYTES => Item::Photo(jpeg),
        Ok(_) => Item::Document(file.to_path_buf()),
        Err(e) => {
            warn!("Could not convert {} to JPEG ({e}), sending it as a file", file.display());
            Item::Document(file.to_path_buf())
        }
    }
}

/// Re-encode as a JPEG of at most PHOTO_EDGE on the long side, never upscaled.
async fn to_jpeg(src: &Path, work: &Path, config: &Config) -> Result<PathBuf, BoxError> {
    let dest = work.join("photo.jpg");
    let scale = format!(
        "scale=w='min({PHOTO_EDGE},iw)':h='min({PHOTO_EDGE},ih)':force_original_aspect_ratio=decrease"
    );
    run_ffmpeg(
        &[
            "-i".as_ref(), src.as_ref(),
            "-frames:v".as_ref(), "1".as_ref(),
            "-vf".as_ref(), scale.as_ref(),
            "-q:v".as_ref(), "2".as_ref(),
            dest.as_ref(),
        ],
        config,
    )
    .await?;
    if file_size(&dest) == 0 {
        return Err("conversion produced an empty file".into());
    }
    Ok(dest)
}

// --- Sending -------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Batch {
    Album(Vec<usize>),
    Single(usize),
}

/// Group items for sending, in post order. Consecutive photos and videos form
/// albums of at most ten whose files together stay within one upload's limit
/// (the limit is per file, but keeping a whole album under it avoids relying on
/// how the server treats large multi-file requests). A run of one is sent on
/// its own, as is anything that cannot go in an album.
fn plan_batches(items: &[(bool, u64)], budget: u64) -> Vec<Batch> {
    fn flush(run: &mut Vec<usize>, out: &mut Vec<Batch>) {
        match run.len() {
            0 => {}
            1 => out.push(Batch::Single(run[0])),
            _ => out.push(Batch::Album(run.clone())),
        }
        run.clear();
    }

    let mut out = Vec::new();
    let mut run = Vec::new();
    let mut run_bytes = 0u64;
    for (i, &(fits_album, size)) in items.iter().enumerate() {
        if !fits_album {
            flush(&mut run, &mut out);
            run_bytes = 0;
            out.push(Batch::Single(i));
            continue;
        }
        if run.len() == ALBUM_MAX || (!run.is_empty() && run_bytes + size > budget) {
            flush(&mut run, &mut out);
            run_bytes = 0;
        }
        run.push(i);
        run_bytes += size;
    }
    flush(&mut run, &mut out);
    out
}

async fn deliver(bot: &Bot, target: &Target, items: &[Item], config: &Config) -> Result<(), BoxError> {
    let sizes: Vec<(bool, u64)> = items
        .iter()
        .map(|item| (item.fits_album(), file_size(item.path())))
        .collect();
    let batches = plan_batches(&sizes, upload_budget(config));

    let mut sent = 0;
    let mut first_error: Option<BoxError> = None;
    let mut note = |result: Result<(), BoxError>, sent: &mut usize| match result {
        Ok(()) => *sent += 1,
        Err(e) => {
            warn!("Could not send an item: {e}");
            first_error.get_or_insert(e);
        }
    };

    for (n, batch) in batches.iter().enumerate() {
        if n > 0 {
            // Telegram allows about one message per second in a chat.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match batch {
            Batch::Album(indices) => {
                let album: Vec<&Item> = indices.iter().map(|&i| &items[i]).collect();
                match send_album(bot, target, &album).await {
                    Ok(()) => sent += album.len(),
                    Err(e) if refused(&e) => {
                        warn!("Album of {} refused ({e}), sending the items one by one", album.len());
                        for item in album {
                            note(send_single(bot, target, item, config).await, &mut sent);
                        }
                    }
                    // The album may have arrived anyway; sending its items
                    // again could post them twice.
                    Err(e) => note(Err(e.into()), &mut sent),
                }
            }
            Batch::Single(i) => note(send_single(bot, target, &items[*i], config).await, &mut sent),
        }
    }

    if sent == 0 {
        return Err(first_error.unwrap_or_else(|| "nothing could be sent".into()));
    }
    if sent < items.len() {
        warn!("Sent {sent} of {} items", items.len());
    }
    Ok(())
}

async fn send_album(bot: &Bot, target: &Target, album: &[&Item]) -> Result<(), RequestError> {
    let media: Vec<InputMedia> = album
        .iter()
        .filter_map(|item| match item {
            Item::Photo(path) => Some(InputMedia::Photo(InputMediaPhoto::new(InputFile::file(
                path.clone(),
            )))),
            Item::Video { path, width, height, duration } => {
                let mut video =
                    InputMediaVideo::new(InputFile::file(path.clone())).supports_streaming(true);
                if let Some(w) = width.and_then(|w| u16::try_from(w).ok()) {
                    video = video.width(w);
                }
                if let Some(h) = height.and_then(|h| u16::try_from(h).ok()) {
                    video = video.height(h);
                }
                if let Some(d) = duration.filter(|d| *d > 0.0) {
                    video = video.duration(d.round().min(u16::MAX as f64) as u16);
                }
                Some(InputMedia::Video(video))
            }
            Item::Animation(_) | Item::Document(_) => None,
        })
        .collect();

    let mut req = bot.send_media_group(target.chat, media);
    if let Some(id) = target.reply_to {
        req = req.reply_parameters(ReplyParameters::new(id));
    }
    with_retry(|| req.send_ref()).await.map(|_| ())
}

async fn send_single(bot: &Bot, target: &Target, item: &Item, config: &Config) -> Result<(), BoxError> {
    match item {
        Item::Photo(path) => send_photo_file(bot, target, path).await,
        Item::Video { path, .. } => send_video_file(bot, target, path, config).await,
        Item::Animation(path) => send_animation_file(bot, target, path).await,
        Item::Document(path) => send_document_file(bot, target, path, None).await,
    }
}

async fn send_photo_file(bot: &Bot, target: &Target, path: &Path) -> Result<(), BoxError> {
    let mut req = bot.send_photo(target.chat, InputFile::file(path.to_path_buf()));
    if let Some(id) = target.reply_to {
        req = req.reply_parameters(ReplyParameters::new(id));
    }
    // Telegram can still refuse a photo it fails to process (an unusual JPEG,
    // say); as a file it gets through.
    send_or_document(bot, target, path, None, req).await
}

async fn send_animation_file(bot: &Bot, target: &Target, path: &Path) -> Result<(), BoxError> {
    let mut req = bot.send_animation(target.chat, InputFile::file(path.to_path_buf()));
    if let Some(id) = target.reply_to {
        req = req.reply_parameters(ReplyParameters::new(id));
    }
    send_or_document(bot, target, path, None, req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn albums_hold_at_most_ten_items() {
        let items = vec![(true, MB); 12];
        assert_eq!(
            plan_batches(&items, 49 * MB),
            vec![Batch::Album((0..10).collect()), Batch::Album(vec![10, 11])]
        );
        let items = vec![(true, MB); 11];
        assert_eq!(
            plan_batches(&items, 49 * MB),
            vec![Batch::Album((0..10).collect()), Batch::Single(10)]
        );
    }

    #[test]
    fn non_album_items_split_runs_and_keep_their_place() {
        let items = [(true, MB), (true, MB), (false, MB), (true, MB), (true, MB), (false, MB)];
        assert_eq!(
            plan_batches(&items, 49 * MB),
            vec![
                Batch::Album(vec![0, 1]),
                Batch::Single(2),
                Batch::Album(vec![3, 4]),
                Batch::Single(5),
            ]
        );
    }

    #[test]
    fn albums_stay_within_the_upload_budget() {
        let items = [(true, 30 * MB), (true, 30 * MB), (true, 5 * MB), (true, 5 * MB)];
        assert_eq!(
            plan_batches(&items, 49 * MB),
            vec![Batch::Single(0), Batch::Album(vec![1, 2, 3])]
        );
        assert_eq!(plan_batches(&[(true, MB)], 49 * MB), vec![Batch::Single(0)]);
        assert_eq!(plan_batches(&[], 49 * MB), vec![]);
    }

    #[test]
    fn only_small_jpeg_and_png_go_out_unchanged() {
        let info = |codec: &str, w, h| ImageInfo {
            codec: Some(codec.to_string()),
            width: Some(w),
            height: Some(h),
        };
        assert!(photo_ok_as_is(&info("mjpeg", 1080, 1350), 400_000));
        assert!(photo_ok_as_is(&info("png", 2000, 2000), 3 * MB));
        assert!(!photo_ok_as_is(&info("webp", 1080, 1350), 400_000));
        assert!(!photo_ok_as_is(&info("hevc", 1080, 1350), 400_000));
        assert!(!photo_ok_as_is(&info("mjpeg", 8000, 6000), 4 * MB));
        assert!(!photo_ok_as_is(&info("png", 3000, 3000), 11 * MB));
        assert!(!photo_ok_as_is(&ImageInfo::default(), 1000));
        assert!(aspect_ratio(100, 2500) > PHOTO_MAX_RATIO);
        assert!(aspect_ratio(1080, 1350) < PHOTO_MAX_RATIO);
    }

    #[test]
    fn downloaded_files_follow_gallery_dl_output_order() {
        let dir = TempDir::new().unwrap();
        for name in ["b.jpg", "a.mp4", "c.jpg"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let p = |n: &str| dir.path().join(n);
        let stdout = format!(
            "{}\n{}\n# {}\n{}\n/elsewhere/x.jpg\nnot a path\n",
            p("b.jpg").display(),
            p("a.mp4").display(),
            p("c.jpg").display(),
            p("b.jpg").display(),
        );
        assert_eq!(downloaded_files(&stdout, dir.path()), vec![p("b.jpg"), p("a.mp4"), p("c.jpg")]);
    }

    /// Needs ffmpeg/ffprobe on PATH: `cargo test -- --ignored photos`.
    #[tokio::test]
    #[ignore = "needs ffmpeg"]
    async fn photos_are_made_telegram_safe() {
        let dir = TempDir::new().unwrap();
        let config = Config::from_env();
        let make = |name: &str, size: &str, extra: &[&str]| {
            let out = dir.path().join(name);
            let status = std::process::Command::new("ffmpeg")
                .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg(format!("testsrc=size={size}:duration=1"))
                .args(["-frames:v", "1"])
                .args(extra)
                .arg(&out)
                .status()
                .unwrap();
            assert!(status.success(), "could not make {name}");
            out
        };

        // An ordinary Instagram JPEG goes out untouched.
        let jpg = make("post.jpg", "1080x1350", &[]);
        let work = dir.path().join("w1");
        std::fs::create_dir_all(&work).unwrap();
        assert!(matches!(prepare_photo(&jpg, &work, &config).await, Item::Photo(p) if p == jpg));

        // WebP becomes a JPEG photo.
        let webp = make("pic.webp", "1200x800", &[]);
        let work = dir.path().join("w2");
        std::fs::create_dir_all(&work).unwrap();
        match prepare_photo(&webp, &work, &config).await {
            Item::Photo(p) => {
                assert_eq!(extension(&p), "jpg");
                let info = probe_image(&config.ffprobe_bin, &p).await;
                assert_eq!((info.width, info.height), (Some(1200), Some(800)));
            }
            other => panic!("expected a photo, got {other:?}"),
        }

        // Too large for sendPhoto (width + height > 10000): scaled into 2560.
        let big = make("big.png", "9000x3000", &[]);
        let work = dir.path().join("w3");
        std::fs::create_dir_all(&work).unwrap();
        match prepare_photo(&big, &work, &config).await {
            Item::Photo(p) => {
                let info = probe_image(&config.ffprobe_bin, &p).await;
                assert_eq!(info.codec.as_deref(), Some("mjpeg"));
                assert_eq!(info.width, Some(2560));
                assert!(info.height.unwrap() <= 854);
            }
            other => panic!("expected a photo, got {other:?}"),
        }

        // A PNG with transparency within limits stays a PNG.
        let alpha = make("alpha.png", "640x480", &["-vf", "format=rgba"]);
        let work = dir.path().join("w4");
        std::fs::create_dir_all(&work).unwrap();
        assert!(matches!(prepare_photo(&alpha, &work, &config).await, Item::Photo(p) if p == alpha));

        // A long screenshot (ratio > 20) goes out as a file.
        let tall = make("tall.png", "100x2500", &[]);
        let work = dir.path().join("w5");
        std::fs::create_dir_all(&work).unwrap();
        assert!(matches!(prepare_photo(&tall, &work, &config).await, Item::Document(_)));

        // Odd sizes survive the JPEG encoder.
        let odd = make("odd.webp", "1001x777", &[]);
        let work = dir.path().join("w6");
        std::fs::create_dir_all(&work).unwrap();
        assert!(matches!(prepare_photo(&odd, &work, &config).await, Item::Photo(_)));
    }
}
