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
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod system;

use std::io::Write;

struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let _ = writeln!(
            std::io::stderr(),
            "{seconds} {} {}",
            record.level(),
            record.args()
        );
    }

    fn flush(&self) {}
}

const USAGE: &str =
    "usage: cabinet-helper daemon --uid <uid> | install --uid <uid> | uninstall | version";

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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
