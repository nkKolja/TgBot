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
    println!("  Update:    tgbot service update");
    println!("  Uninstall: tgbot service uninstall");
}

fn do_update() {
    let dst = plist_dst();
    if !dst.exists() {
        eprintln!("Service not installed. Run 'tgbot service install' first.");
        std::process::exit(1);
    }

    stop_service();
    build(&repo_dir());
    start_service();

    println!();
    println!("Updated and restarted.");
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
            eprintln!("Usage: tgbot service {{install|update|uninstall|status}}");
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
