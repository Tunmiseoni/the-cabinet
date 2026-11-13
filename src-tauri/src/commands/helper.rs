use crate::error::CommandResult;
use cabinet_helper::protocol::Entry;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperStatus {
    pub supported: bool,
    pub installed: bool,
    pub ready: bool,
    pub expected_version: u32,
    pub entries: Vec<Entry>,
    pub error: Option<String>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod system {
    use super::HelperStatus;
    use crate::cartridge::privileged;
    use cabinet_helper::protocol::VERSION;
    use cabinet_helper::system::INSTALLED;
    use std::path::{Path, PathBuf};

    const BUNDLED: &str = "cabinet-helper";
    #[cfg(target_os = "macos")]
    const SCRIPT: &[&str] = &[
        "on run argv",
        "set command to \"\"",
        "repeat with piece in rest of argv",
        "set command to command & quoted form of piece & \" \"",
        "end repeat",
        "do shell script command with prompt (item 1 of argv) with administrator privileges",
        "end run",
    ];

    pub fn status() -> HelperStatus {
        let helper = privileged::system_socket();
        let installed = !helper.absent();
        let entries = if installed {
            helper.connect().and_then(|mut client| client.status())
        } else {
            Ok(Vec::new())
        };
        HelperStatus {
            supported: true,
            installed,
            ready: installed && entries.is_ok(),
            expected_version: VERSION,
            error: entries.as_ref().err().cloned(),
            entries: entries.unwrap_or_default(),
        }
    }

    fn bundled() -> crate::error::Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let path = exe.with_file_name(BUNDLED);
        if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "the helper program is missing from this build ({})",
                path.display()
            )
            .into())
        }
    }

    #[cfg(target_os = "macos")]
    fn as_admin(prompt: &str, program: &Path, args: &[&str]) -> crate::error::Result<()> {
        let mut command = crate::process::command("/usr/bin/osascript");
        for line in SCRIPT {
            command.args(["-e", line]);
        }
        let output = command.arg(prompt).arg(program).args(args).output()?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("-128") {
            return Err("the administrator prompt was cancelled".into());
        }
        Err(format!("the helper could not be changed: {}", stderr.trim()).into())
    }

    #[cfg(target_os = "linux")]
    fn as_admin(prompt: &str, program: &Path, args: &[&str]) -> crate::error::Result<()> {
        log::info!("{prompt}");
        let output = crate::process::command(PKEXEC)
            .arg(program)
            .args(args)
            .output()
            .map_err(|err| format!("cannot run {PKEXEC}: {err}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        match output.status.code() {
            Some(126) => Err("the administrator prompt was cancelled".into()),
            Some(127) => Err(format!(
                "the administrator prompt was refused or no authentication agent is running: {}",
                stderr.trim()
            )
            .into()),
            _ => Err(format!("the helper could not be changed: {}", stderr.trim()).into()),
        }
    }

    #[cfg(target_os = "linux")]
    const PKEXEC: &str = "/usr/bin/pkexec";

    #[cfg(target_os = "linux")]
    struct Staged(PathBuf);

    #[cfg(target_os = "linux")]
    impl Staged {
        fn copy(source: &Path) -> crate::error::Result<Self> {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            let dir =
                std::env::temp_dir().join(format!("the-cabinet-helper-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            let staged = Self(dir);
            let program = staged.program();
            std::fs::copy(source, &program)?;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
            Ok(staged)
        }

        fn program(&self) -> PathBuf {
            self.0.join(BUNDLED)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const INSTALL_PROMPT: &str = "The Cabinet wants to install its helper, which makes the system changes a cartridge was approved for and undoes them afterwards.";

    #[cfg(target_os = "macos")]
    pub fn install() -> crate::error::Result<()> {
        let uid = unsafe { libc::getuid() }.to_string();
        as_admin(INSTALL_PROMPT, &bundled()?, &["install", "--uid", &uid])
    }

    #[cfg(target_os = "linux")]
    pub fn install() -> crate::error::Result<()> {
        let uid = unsafe { libc::getuid() }.to_string();
        let staged = Staged::copy(&bundled()?)?;
        as_admin(
            INSTALL_PROMPT,
            &staged.program(),
            &["install", "--uid", &uid],
        )
    }

    pub fn uninstall() -> crate::error::Result<()> {
        as_admin(
            "The Cabinet wants to remove its helper and undo every change it still holds.",
            Path::new(INSTALLED),
            &["uninstall"],
        )
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod system {
    use super::HelperStatus;

    const UNAVAILABLE: &str = "The Cabinet's helper is not available on this system yet";

    pub fn status() -> HelperStatus {
        HelperStatus {
            supported: false,
            installed: false,
            ready: false,
            expected_version: cabinet_helper::protocol::VERSION,
            entries: Vec::new(),
            error: None,
        }
    }

    pub fn install() -> crate::error::Result<()> {
        Err(UNAVAILABLE.into())
    }

    pub fn uninstall() -> crate::error::Result<()> {
        Err(UNAVAILABLE.into())
    }
}

#[tauri::command(async)]
pub fn helper_status() -> HelperStatus {
    system::status()
}

#[tauri::command(async)]
pub fn helper_install() -> CommandResult<HelperStatus> {
    system::install()?;
    Ok(system::status())
}

#[tauri::command(async)]
pub fn helper_uninstall() -> CommandResult<HelperStatus> {
    system::uninstall()?;
    Ok(system::status())
}

pub fn sweep_on_startup() {
    let Some(helper) = crate::cartridge::privileged::system() else {
        return;
    };
    tauri::async_runtime::spawn_blocking(move || match helper.sweep() {
        Ok(0) => {}
        Ok(undone) => log::info!("the helper undid {undone} change(s) left by an earlier run"),
        Err(err) => log::warn!("cannot ask the helper to clean up: {err}"),
    });
}
