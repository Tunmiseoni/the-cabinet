pub trait PortGrant: Send {
    fn granted(&self, program: &str) -> Result<bool, String>;
    fn grant(&self, program: &str) -> Result<(), String>;
    fn revoke(&self, program: &str) -> Result<(), String>;
}

pub const ATTRIBUTE: &str = "security.capability";
const CAP_NET_BIND_SERVICE: u32 = 10;
const REVISION_MASK: u32 = 0xFF00_0000;
const REVISION_2: u32 = 0x0200_0000;
const REVISION_3: u32 = 0x0300_0000;
const FLAG_EFFECTIVE: u32 = 0x0000_0001;

pub fn encode() -> [u8; 20] {
    let mut value = [0u8; 20];
    value[0..4].copy_from_slice(&(REVISION_2 | FLAG_EFFECTIVE).to_le_bytes());
    value[4..8].copy_from_slice(&(1u32 << CAP_NET_BIND_SERVICE).to_le_bytes());
    value
}

pub fn binds_low_ports(value: &[u8]) -> bool {
    let word = |at: usize| {
        value
            .get(at..at + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let (Some(magic), Some(permitted)) = (word(0), word(4)) else {
        return false;
    };
    matches!(magic & REVISION_MASK, REVISION_2 | REVISION_3)
        && magic & FLAG_EFFECTIVE != 0
        && permitted & (1 << CAP_NET_BIND_SERVICE) != 0
}

#[cfg(target_os = "linux")]
pub struct FileCapability;

#[cfg(target_os = "linux")]
impl FileCapability {
    fn names(program: &str) -> Result<(std::ffi::CString, std::ffi::CString), String> {
        Ok((
            std::ffi::CString::new(program).map_err(|_| "the program path holds a NUL")?,
            std::ffi::CString::new(ATTRIBUTE).expect("the attribute name has no NUL"),
        ))
    }

    fn read(program: &str) -> Result<Option<Vec<u8>>, String> {
        let (path, name) = Self::names(program)?;
        let mut value = vec![0u8; 64];
        let read = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr() as *mut libc::c_void,
                value.len(),
            )
        };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENODATA) {
                return Ok(None);
            }
            return Err(format!("cannot read the capabilities of {program}: {err}"));
        }
        value.truncate(read as usize);
        Ok(Some(value))
    }
}

#[cfg(target_os = "linux")]
impl PortGrant for FileCapability {
    fn granted(&self, program: &str) -> Result<bool, String> {
        match Self::read(program)? {
            None => Ok(false),
            Some(value) if binds_low_ports(&value) => Ok(true),
            Some(_) => Err(format!(
                "{program} already carries other file capabilities, so The Cabinet leaves it alone"
            )),
        }
    }

    fn grant(&self, program: &str) -> Result<(), String> {
        let (path, name) = Self::names(program)?;
        let value = encode();
        let written = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                0,
            )
        };
        if written != 0 {
            return Err(format!(
                "cannot let {program} bind low ports: {}",
                std::io::Error::last_os_error()
            ));
        }
        if self.granted(program)? {
            Ok(())
        } else {
            Err(format!("{program} did not take the low-port capability"))
        }
    }

    fn revoke(&self, program: &str) -> Result<(), String> {
        let (path, name) = Self::names(program)?;
        if unsafe { libc::removexattr(path.as_ptr(), name.as_ptr()) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ENODATA)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("cannot take low ports back from {program}: {err}"));
            }
        }
        match Self::read(program)? {
            None => Ok(()),
            Some(_) => Err(format!("{program} kept its low-port capability")),
        }
    }
}

#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct FakeGrant {
        pub programs: Arc<Mutex<Vec<String>>>,
    }

    impl PortGrant for FakeGrant {
        fn granted(&self, program: &str) -> Result<bool, String> {
            Ok(self
                .programs
                .lock()
                .unwrap()
                .iter()
                .any(|held| held == program))
        }

        fn grant(&self, program: &str) -> Result<(), String> {
            self.programs.lock().unwrap().push(program.to_string());
            Ok(())
        }

        fn revoke(&self, program: &str) -> Result<(), String> {
            self.programs.lock().unwrap().retain(|held| held != program);
            Ok(())
        }
    }

    #[test]
    fn the_capability_is_bind_service_alone_and_effective() {
        let value = encode();
        assert_eq!(value[..8], [0x01, 0, 0, 0x02, 0x00, 0x04, 0, 0]);
        assert!(value[8..].iter().all(|byte| *byte == 0));
        assert!(binds_low_ports(&value));
        let mut not_effective = value;
        not_effective[0] = 0;
        assert!(!binds_low_ports(&not_effective));
        let mut other = value;
        other[5] = 0;
        other[4] = 1;
        assert!(!binds_low_ports(&other));
        assert!(!binds_low_ports(&value[..6]));
    }
}
