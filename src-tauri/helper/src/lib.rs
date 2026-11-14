pub mod anchors;
pub mod client;
pub mod engine;
pub mod firewall;
pub mod hosts;
pub mod journal;
pub mod lowports;
pub mod menu;
pub mod protocol;
pub mod server;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub mod system;
#[cfg(windows)]
pub mod win;

use std::io::Write;

struct StderrLog;

#[cfg(windows)]
static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();

#[cfg(windows)]
const MAX_LOG: u64 = 1024 * 1024;

#[cfg(windows)]
pub(crate) fn log_to(path: &std::path::Path) {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_LOG) {
        let _ = std::fs::remove_file(path);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => {
            let _ = LOG_FILE.set(std::sync::Mutex::new(file));
        }
        Err(err) => log::warn!("cannot log to {}: {err}", path.display()),
    }
}

impl log::Log for StderrLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let line = format!("{seconds} {} {}\n", record.level(), record.args());
        #[cfg(windows)]
        if let Some(file) = LOG_FILE.get() {
            let mut file = file.lock().unwrap_or_else(|err| err.into_inner());
            let _ = file.write_all(line.as_bytes());
            return;
        }
        let _ = std::io::stderr().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

#[cfg(not(windows))]
const USAGE: &str =
    "usage: cabinet-helper daemon --uid <uid> | install --uid <uid> | uninstall | version";
#[cfg(windows)]
const USAGE: &str =
    "usage: cabinet-helper daemon --sid <sid> | install --sid <sid> | uninstall | version";

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn dispatch(args: &[&str]) -> Result<(), String> {
    let uid = |value: &str| {
        value
            .parse::<u32>()
            .map_err(|_| format!("\"{value}\" is not a user id"))
    };
    match args {
        ["daemon", "--uid", value] => system::daemon(uid(value)?),
        ["install", "--uid", value] => system::install(uid(value)?),
        ["uninstall"] => system::uninstall(),
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(windows)]
fn dispatch(args: &[&str]) -> Result<(), String> {
    match args {
        ["daemon", "--sid", sid] => system::daemon(sid),
        ["install", "--sid", sid] => system::install(sid),
        ["uninstall"] => system::uninstall(),
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn dispatch(args: &[&str]) -> Result<(), String> {
    match args {
        ["daemon", ..] | ["install", ..] | ["uninstall"] => {
            Err("the helper is not available on this system yet".to_string())
        }
        _ => Err(USAGE.to_string()),
    }
}

pub fn main() -> std::process::ExitCode {
    static LOGGER: StderrLog = StderrLog;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if args == ["version"] {
        let _ = writeln!(std::io::stdout(), "{}", protocol::VERSION);
        return std::process::ExitCode::SUCCESS;
    }
    match dispatch(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            log::error!("{err}");
            std::process::ExitCode::FAILURE
        }
    }
}
