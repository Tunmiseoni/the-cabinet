use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(crate) fn home(uid: u32) -> Option<PathBuf> {
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut record,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || record.pw_dir.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr(record.pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}

pub(crate) fn require_root() -> Result<(), String> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err("this needs to run as root".to_string())
    }
}

pub(crate) fn place(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    let temp = path.with_extension("installing");
    let _ = std::fs::remove_file(&temp);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.write_all(content)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    written.map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot write {}: {err}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_home_folder_of_a_user() {
        let me = unsafe { libc::getuid() };
        assert_eq!(
            home(me).unwrap(),
            PathBuf::from(std::env::var_os("HOME").unwrap())
        );
    }
}
