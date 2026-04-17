use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const PLIST_NAME: &str = "com.user.tgbot-rust";
const PLIST_TEMPLATE: &str = "com.user.tgbot-rust.plist";

fn repo_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        // exe is at target/release/tgbot or target/debug/tgbot
        .and_then(|p| p.parent()?.parent()?.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| env::current_dir().expect("cannot determine repo dir"))
}

fn plist_dst() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home dir")
        .join("Library/LaunchAgents")
        .join(format!("{PLIST_NAME}.plist"))
}

fn service_is_loaded() -> bool {
    Command::new("launchctl")
        .args(["list", PLIST_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn stop_service() {
    if service_is_loaded() {
        println!("Stopping service...");
        let dst = plist_dst();
        let _ = Command::new("launchctl")
            .args(["unload", &dst.to_string_lossy()])
            .status();
    }
}

fn start_service() {
    println!("Starting service...");
    let dst = plist_dst();
    let status = Command::new("launchctl")
        .args(["load", &dst.to_string_lossy()])
        .status()
        .expect("failed to run launchctl");
    if !status.success() {
        eprintln!("launchctl load failed");
        std::process::exit(1);
    }
}

fn build(repo: &Path) {
    println!("Building release binary...");
    let status = Command::new("cargo")
        .args(["build", "--release", "--manifest-path"])
        .arg(repo.join("Cargo.toml"))
        .status()
        .expect("failed to run cargo");
    if !status.success() {
        eprintln!("Build failed");
        std::process::exit(1);
    }
}

fn do_install() {
    let repo = repo_dir();
    let plist_src = repo.join(PLIST_TEMPLATE);

    if !plist_src.exists() {
        eprintln!("Error: {} not found", plist_src.display());
        std::process::exit(1);
    }

    build(&repo);

    println!("Installing service plist...");
    let template = fs::read_to_string(&plist_src).expect("cannot read plist template");
    let content = template.replace("/PATH/TO/tgbot/rust", &repo.to_string_lossy());
    let dst = plist_dst();
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&dst, content).expect("cannot write plist");

    start_service();

    println!();
    println!("Done. Bot is running as a background service.");
    println!("  Logs:   {}/tgbot.log", repo.display());
    println!("  Errors: {}/tgbot-error.log", repo.display());
    println!();
    println!("  Update:    cargo run -- service update");
    println!("  Uninstall: cargo run -- service uninstall");
}

fn find_brew() -> Option<PathBuf> {
    for p in ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"] {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn update_tools() {
    println!("Updating external tools...");

    let brew = find_brew();

    // Resolve yt-dlp the same way config.rs does
    let home = env::var("HOME").unwrap_or_default();
    let ytdlp = [
        "/opt/homebrew/bin/yt-dlp",
        "/usr/local/bin/yt-dlp",
        &format!("{home}/.local/bin/yt-dlp"),
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
    .unwrap_or_else(|| PathBuf::from("yt-dlp"));

    // Detect if yt-dlp is brew-managed
    let is_brew_ytdlp = brew.is_some()
        && ytdlp.to_string_lossy().contains("/homebrew/")
        || ytdlp.to_string_lossy().starts_with("/usr/local/bin");

    if is_brew_ytdlp {
        // brew-managed: `yt-dlp --update` won't work
        let brew = brew.as_ref().unwrap();
        println!("  brew upgrade yt-dlp");
        match Command::new(brew).args(["upgrade", "yt-dlp"]).status() {
            Ok(s) if s.success() => println!("  yt-dlp: ok"),
            Ok(s) => eprintln!("  brew upgrade yt-dlp exited with {s} (may already be latest)"),
            Err(e) => eprintln!("  brew upgrade yt-dlp failed: {e}"),
        }
    } else {
        // pip / pipx / standalone: self-update
        println!("  yt-dlp --update");
        match Command::new(&ytdlp).arg("--update").status() {
            Ok(s) if s.success() => println!("  yt-dlp: ok"),
            Ok(s) => eprintln!("  yt-dlp --update exited with {s}"),
            Err(e) => eprintln!("  yt-dlp --update failed: {e}"),
        }
    }

    // brew upgrade ffmpeg (ffprobe is part of the ffmpeg package)
    if let Some(brew) = &brew {
        println!("  brew upgrade ffmpeg");
        match Command::new(brew).args(["upgrade", "ffmpeg"]).status() {
            Ok(s) if s.success() => println!("  ffmpeg/ffprobe: ok"),
            Ok(s) => eprintln!("  brew upgrade ffmpeg exited with {s} (may already be latest)"),
            Err(e) => eprintln!("  brew upgrade ffmpeg failed: {e}"),
        }
    } else {
        println!("  brew not found, skipping ffmpeg update");
    }

    println!();
}

fn do_update() {
    let installed = plist_dst().exists();

    if installed {
        stop_service();
    }

    update_tools();
    build(&repo_dir());

    if installed {
        start_service();
        println!();
        println!("Updated and restarted.");
    } else {
        println!();
        println!("Updated. Service is not installed, skipping restart.");
    }
}

fn do_uninstall() {
    stop_service();

    let dst = plist_dst();
    if dst.exists() {
        fs::remove_file(&dst).expect("cannot remove plist");
        println!("Removed {}", dst.display());
    }

    println!("Service uninstalled.");
}

fn do_status() {
    if service_is_loaded() {
        println!("Service is loaded.");
        let _ = Command::new("launchctl")
            .args(["list", PLIST_NAME])
            .status();
    } else {
        println!("Service is not loaded.");
    }
}

pub fn run(args: &[String]) -> ExitCode {
    let subcmd = args.first().map(String::as_str);

    match subcmd {
        Some("install") => do_install(),
        Some("update") => do_update(),
        Some("uninstall") => do_uninstall(),
        Some("status") => do_status(),
        _ => {
            eprintln!("Usage: cargo run -- service {{install|update|uninstall|status}}");
            eprintln!();
            eprintln!("  install    Build and register the launchd service");
            eprintln!("  update     Rebuild and restart the running service");
            eprintln!("  uninstall  Stop the service and remove the plist");
            eprintln!("  status     Show whether the service is loaded/running");
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}
