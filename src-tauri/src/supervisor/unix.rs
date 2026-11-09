use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

pub(super) fn configure(command: &mut Command) {
    command.process_group(0);
}

pub(super) struct Tree {
    group: libc::pid_t,
}

impl Tree {
    pub(super) fn attach(child: &Child) -> crate::error::Result<Self> {
        let group = libc::pid_t::try_from(child.id())
            .map_err(|_| format!("pid {} out of range", child.id()))?;
        Ok(Self { group })
    }

    pub(super) fn terminate(&self) {
        self.signal(libc::SIGTERM);
    }

    pub(super) fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    fn signal(&self, signal: libc::c_int) {
        unsafe {
            libc::killpg(self.group, signal);
        }
    }
}
