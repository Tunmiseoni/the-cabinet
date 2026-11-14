use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    GetNamedSecurityInfoW, GetSecurityInfo, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;

pub const SYSTEM: &str = "S-1-5-18";
pub const ADMINISTRATORS: &str = "S-1-5-32-544";

pub fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

pub fn error(what: &str, code: u32) -> String {
    format!("{what}: {}", std::io::Error::from_raw_os_error(code as i32))
}

unsafe fn read_wide(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *text.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
}

struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

unsafe fn sid_text(sid: PSID) -> Result<String, String> {
    let mut text: PWSTR = std::ptr::null_mut();
    if ConvertSidToStringSidW(sid, &mut text) == 0 {
        return Err(last_error("cannot read a security identifier"));
    }
    let _free = Local(text.cast());
    Ok(read_wide(text))
}

pub fn check_sid(text: &str) -> Result<(), String> {
    let mut sid: PSID = std::ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid) } == 0 {
        return Err(format!("\"{text}\" is not a security identifier"));
    }
    let _free = Local(sid);
    Ok(())
}

pub fn with_sid<T>(text: &str, work: impl FnOnce(PSID) -> T) -> Result<T, String> {
    let mut sid: PSID = std::ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid) } == 0 {
        return Err(format!("\"{text}\" is not a security identifier"));
    }
    let _free = Local(sid);
    Ok(work(sid))
}

pub fn current_user() -> Result<String, String> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("cannot open this process's token"));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(last_error("cannot read this process's user"));
    }
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    unsafe { sid_text(user.User.Sid) }
}

pub struct Descriptor(Local);

unsafe impl Send for Descriptor {}

impl Descriptor {
    pub fn parse(sddl: &str) -> Result<Self, String> {
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error(&format!("cannot build the access list {sddl}")));
        }
        Ok(Self(Local(descriptor)))
    }

    pub fn access(&self) -> Result<String, String> {
        unsafe { access_text(self.0 .0) }
    }

    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: 0,
        }
    }
}

pub fn owner_of(path: &Path) -> Result<String, String> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(error(
            &format!("cannot read the owner of {}", path.display()),
            status,
        ));
    }
    let _free = Local(descriptor);
    unsafe { sid_text(owner) }
}

pub fn set_owner(path: &Path, sid: &str) -> Result<(), String> {
    let mut owner: PSID = std::ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(wide(sid).as_ptr(), &mut owner) } == 0 {
        return Err(format!("\"{sid}\" is not a security identifier"));
    }
    let _free = Local(owner);
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            owner,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(error(
            &format!("cannot hand {} to {sid}", path.display()),
            status,
        ));
    }
    Ok(())
}

pub fn access_of(handle: &impl AsRawHandle) -> Result<String, String> {
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(error("cannot read an access list", status));
    }
    let _free = Local(descriptor);
    unsafe { access_text(descriptor) }
}

unsafe fn access_text(descriptor: *mut c_void) -> Result<String, String> {
    let mut text: PWSTR = std::ptr::null_mut();
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("cannot write an access list as text"));
    }
    let _text = Local(text.cast());
    Ok(unsafe { read_wide(text) })
}

pub fn known_folder(id: &GUID) -> Result<PathBuf, String> {
    let mut path: PWSTR = std::ptr::null_mut();
    let status = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
    let text = unsafe { read_wide(path) };
    unsafe { CoTaskMemFree(path as *const c_void) };
    if status < 0 || text.is_empty() {
        return Err(format!("cannot find a system folder (error {status:#x})"));
    }
    Ok(PathBuf::from(text))
}

pub fn windows_dir() -> Result<PathBuf, String> {
    let mut buffer = vec![0u16; 1024];
    let len = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 || len as usize >= buffer.len() {
        return Err(last_error("cannot find the Windows folder"));
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..len as usize],
    )))
}

pub fn machine_text(key: &str, value: &str) -> Option<String> {
    let mut buffer = vec![0u16; 32 * 1024];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide(key).as_ptr(),
            wide(value).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let text = unsafe { read_wide(buffer.as_ptr()) };
    (!text.is_empty()).then_some(text)
}

pub fn profile(sid: &str) -> Option<PathBuf> {
    machine_text(
        &format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}"),
        "ProfileImagePath",
    )
    .map(PathBuf::from)
}

pub fn plain(path: &str) -> &str {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_current_user_and_its_profile() {
        let me = current_user().unwrap();
        assert!(me.starts_with("S-1-"), "{me}");
        check_sid(&me).unwrap();
        assert!(check_sid("not a sid").is_err());
        let home = profile(&me).unwrap();
        assert!(home.is_dir(), "{}", home.display());
    }

    #[test]
    fn finds_the_system_folders() {
        assert!(windows_dir().unwrap().join("System32").is_dir());
        use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, FOLDERID_ProgramFiles};
        assert!(known_folder(&FOLDERID_ProgramFiles).unwrap().is_dir());
        assert!(known_folder(&FOLDERID_ProgramData).unwrap().is_dir());
    }

    #[test]
    fn reads_the_owner_of_a_file() {
        let path = std::env::temp_dir().join(format!("cabinet-owner-{}", std::process::id()));
        std::fs::write(&path, "x").unwrap();
        let owner = owner_of(&path);
        let _ = std::fs::remove_file(&path);
        let owner = owner.unwrap();
        assert!(
            owner == current_user().unwrap() || owner == ADMINISTRATORS,
            "{owner}"
        );
    }

    #[test]
    fn strips_the_long_path_prefix_from_a_drive_path() {
        assert_eq!(plain(r"\\?\C:\Users\me\x.exe"), r"C:\Users\me\x.exe");
        assert_eq!(plain(r"\\?\UNC\server\share"), r"\\?\UNC\server\share");
        assert_eq!(plain(r"C:\x.exe"), r"C:\x.exe");
    }
}
