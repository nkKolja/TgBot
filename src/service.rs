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

/// Homebrew ships no ready-made packages for macOS 12 and older ("Tier 3"), so
/// `brew upgrade` there compiles from source: LLVM, Rust and Python for a tool
/// like gallery-dl, which takes hours on old hardware and often fails anyway.
fn brew_builds_from_source() -> bool {
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .split('.')
                .next()?
                .parse::<u32>()
                .ok()
        })
        .is_some_and(|major| major <= 12)
}

/// Whether `manager` lists `name` among what it installed. `brew list
/// --versions NAME` prints a line only when brew owns NAME; `uv tool list`
/// prints one "NAME vX.Y" line per installed tool.
fn owns(manager: &Path, args: &[&str], name: &str) -> bool {
    Command::new(manager)
        .args(args)
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some(name))
        })
        .unwrap_or(false)
}

fn run_step(program: &Path, args: &[&str], label: &str) {
    println!("  {} {}", program.file_name().unwrap_or_default().to_string_lossy(), args.join(" "));
    match Command::new(program).args(args).status() {
        Ok(s) if s.success() => println!("  {label}: ok"),
        Ok(s) => eprintln!("  {label}: exited with {s} (may already be the latest)"),
        Err(e) => eprintln!("  {label}: failed to run: {e}"),
    }
}

/// Update yt-dlp or gallery-dl through whatever installed it.
fn update_downloader(name: &str, brew: Option<&Path>, old_macos: bool, uv: Option<&Path>) {
    if let Some(brew) = brew.filter(|b| owns(b, &["list", "--versions", name], name)) {
        if old_macos {
            println!("  {name} comes from Homebrew, which cannot update it on this macOS.");
            println!("  Switch it to uv: brew uninstall {name} && uv tool install --python 3.13 {name}");
        } else {
            run_step(brew, &["upgrade", name], name);
        }
        return;
    }
    if let Some(uv) = uv.filter(|uv| owns(uv, &["tool", "list"], name)) {
        run_step(uv, &["tool", "upgrade", name], name);
        return;
    }
    let bin = crate::config::which(name);
    if !bin.is_file() {
        println!("  {name} is not installed");
        return;
    }
    if name == "yt-dlp" {
        // Standalone binary or pip: yt-dlp updates itself.
        run_step(&bin, &["--update"], name);
    } else {
        println!("  {name} was not installed by Homebrew or uv; update it the way it was installed");
        println!("  (for example: pip install -U {name})");
    }
}

fn update_tools() {
    println!("Updating external tools...");

    let brew = find_brew();
    let old_macos = brew.is_some() && brew_builds_from_source();
    if old_macos {
        println!("  macOS 12 or older: Homebrew would compile updates from source, so brew upgrades are skipped");
    }
    let uv = Some(crate::config::which("uv")).filter(|p| p.is_file());

    update_downloader("yt-dlp", brew.as_deref(), old_macos, uv.as_deref());
    // gallery-dl fetches photo posts. Sites change often enough that an old
    // copy stops working, so it is kept current along with yt-dlp.
    update_downloader("gallery-dl", brew.as_deref(), old_macos, uv.as_deref());
    if !crate::config::which("gallery-dl").is_file() {
        println!("  Photo posts need gallery-dl: brew install gallery-dl");
        println!("  (on macOS 12 or older: uv tool install --python 3.13 gallery-dl)");
    }

    // ffmpeg/ffprobe come from Homebrew when they come from a package manager at all.
    match &brew {
        Some(brew) if !old_macos => run_step(brew, &["upgrade", "ffmpeg"], "ffmpeg/ffprobe"),
        Some(_) => println!("  ffmpeg: skipped (Homebrew cannot update it on this macOS)"),
        None => println!("  brew not found, skipping ffmpeg update"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ownership_is_read_from_the_package_manager_listing() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = dir.path().join("fake-uv");
        // What `uv tool list` prints.
        fs::write(
            &fake,
            "#!/bin/sh\nprintf 'gallery-dl v1.32.16\\n- gallery-dl\\nyt-dlp v2026.6.9\\n- yt-dlp\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(owns(&fake, &["tool", "list"], "gallery-dl"));
        assert!(owns(&fake, &["tool", "list"], "yt-dlp"));
        assert!(!owns(&fake, &["tool", "list"], "ffmpeg"));
        assert!(!owns(&fake, &["tool", "list"], "gallery"));
        assert!(!owns(&dir.path().join("missing"), &["tool", "list"], "yt-dlp"));
    }
}
