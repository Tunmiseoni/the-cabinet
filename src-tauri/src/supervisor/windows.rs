use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

pub(super) fn configure(_command: &mut Command) {}

pub(super) struct Tree {
    job: HANDLE,
}

unsafe impl Send for Tree {}
unsafe impl Sync for Tree {}

impl Tree {
    pub(super) fn attach(child: &Child) -> crate::error::Result<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(format!(
                "cannot create a job object: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        let tree = Self { job };

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let limited = unsafe {
            SetInformationJobObject(
                tree.job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if limited == 0 {
            return Err(format!(
                "cannot configure the job object: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }

        let assigned =
            unsafe { AssignProcessToJobObject(tree.job, child.as_raw_handle() as HANDLE) };
        if assigned == 0 {
            return Err(format!(
                "cannot assign pid {} to its job object: {}",
                child.id(),
                std::io::Error::last_os_error()
            )
            .into());
        }
        Ok(tree)
    }

    pub(super) fn terminate(&self) {}

    pub(super) fn kill(&self) {
        unsafe {
            TerminateJobObject(self.job, 1);
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.job);
        }
    }
}
