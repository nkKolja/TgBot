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

/// launchd domain of the current login session, e.g. `gui/501`.
fn gui_domain() -> Option<String> {
    let out = Command::new("id").arg("-u").output().ok()?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !uid.is_empty()).then(|| format!("gui/{uid}"))
}

fn quiet(cmd: &mut Command) -> bool {
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn service_is_loaded() -> bool {
    if let Some(domain) = gui_domain() {
        if quiet(Command::new("launchctl").args(["print", &format!("{domain}/{PLIST_NAME}")])) {
            return true;
        }
    }
    // Older macOS, or a session launchd does not expose as gui/<uid>.
    quiet(Command::new("launchctl").args(["list", PLIST_NAME]))
}

fn stop_service() {
    if !service_is_loaded() {
        return;
    }
    println!("Stopping service...");
    let stopped = gui_domain().is_some_and(|domain| {
        quiet(Command::new("launchctl").args(["bootout", &format!("{domain}/{PLIST_NAME}")]))
    });
    if !stopped {
        let dst = plist_dst();
        let _ = Command::new("launchctl")
            .args(["unload", &dst.to_string_lossy()])
            .status();
    }
}

fn start_service() {
    println!("Starting service...");
    let dst = plist_dst();
    let dst = dst.to_string_lossy();
    // `bootstrap` is the supported interface; `load` is deprecated but still
    // the one that works from some non-GUI sessions (e.g. over SSH).
    let started = gui_domain()
        .is_some_and(|domain| quiet(Command::new("launchctl").args(["bootstrap", &domain, &dst])))
        || Command::new("launchctl")
            .args(["load", "-w", &dst])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !started {
        eprintln!("launchctl could not start the service");
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

    // Re-running install must not fail because the service is already loaded.
    stop_service();

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

    // Ask brew whether it owns a tool rather than guessing from the path:
    // /usr/local/bin is brew's prefix on Intel Macs but also where standalone
    // installs land, and brew may not be installed at all.
    let brew_owns = |name: &str| {
        brew.as_ref().filter(|brew| {
            Command::new(brew)
                .args(["list", "--versions", name])
                .output()
                .map(|o| o.status.success() && !o.stdout.trim_ascii().is_empty())
                .unwrap_or(false)
        })
    };

    if let Some(brew) = brew_owns("yt-dlp") {
        // brew-managed: `yt-dlp --update` won't work
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

    // gallery-dl fetches photo posts. Sites change often enough that an old
    // copy stops working, so it is kept current along with yt-dlp.
    if let Some(brew) = brew_owns("gallery-dl") {
        println!("  brew upgrade gallery-dl");
        match Command::new(brew).args(["upgrade", "gallery-dl"]).status() {
            Ok(s) if s.success() => println!("  gallery-dl: ok"),
            Ok(s) => eprintln!("  brew upgrade gallery-dl exited with {s} (may already be latest)"),
            Err(e) => eprintln!("  brew upgrade gallery-dl failed: {e}"),
        }
    } else if crate::config::which("gallery-dl").is_file() {
        println!("  gallery-dl is not from Homebrew; update it the way it was installed");
        println!("  (for example: pip install -U gallery-dl)");
    } else {
        println!("  gallery-dl is not installed; photo posts need it: brew install gallery-dl");
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
        let _ = Command::new("launchctl").args(["list", PLIST_NAME]).status();
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
