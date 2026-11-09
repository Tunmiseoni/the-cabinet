use std::path::{Path, PathBuf};

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub(crate) fn os_hostname() -> Option<String> {
    if let Ok(output) = crate::process::command("hostname").output() {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn instance() -> Option<String> {
    std::env::var("CABINET_INSTANCE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub(crate) fn path_program_on_path(program: &Path) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    program
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| on_path(name).is_some())
        .unwrap_or(false)
}
