#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use crate::contracts::LaunchSpec;
use crate::logging::{self, LineObserver};
use crate::sync::MutexExt;
use std::fs::File;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Capture {
    pub label: String,
    pub sink: Arc<Mutex<File>>,
    pub observer: Option<LineObserver>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
}

#[derive(Default)]
struct ExitCell {
    value: Mutex<Option<Exit>>,
    ready: Condvar,
}

impl ExitCell {
    fn set(&self, exit: Exit) {
        *self.value.lock_or_recover() = Some(exit);
        self.ready.notify_all();
    }

    fn get(&self) -> Option<Exit> {
        *self.value.lock_or_recover()
    }

    fn wait_timeout(&self, timeout: Duration) -> Option<Exit> {
        let deadline = Instant::now() + timeout;
        let mut value = self.value.lock_or_recover();
        loop {
            if let Some(exit) = *value {
                return Some(exit);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            value = self
                .ready
                .wait_timeout(value, left)
                .unwrap_or_else(|err| err.into_inner())
                .0;
        }
    }
}

pub struct Supervised {
    pid: u32,
    tree: Arc<platform::Tree>,
    exit: Arc<ExitCell>,
}

impl Supervised {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn try_exit(&self) -> Option<Exit> {
        self.exit.get()
    }

    pub fn stop_blocking(&self, grace: Duration) -> Option<Exit> {
        if let Some(exit) = self.exit.get() {
            return Some(exit);
        }
        if !grace.is_zero() {
            self.tree.terminate();
            if let Some(exit) = self.exit.wait_timeout(grace) {
                return Some(exit);
            }
        }
        self.tree.kill();
        self.exit
            .wait_timeout(crate::constants::SUPERVISOR_REAP_TIMEOUT)
    }
}

pub fn spawn(spec: &LaunchSpec, capture: Option<Capture>) -> crate::error::Result<Supervised> {
    let mut command = crate::process::command(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(spec.envs.iter().cloned());
    if capture.is_some() {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    platform::configure(&mut command);

    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to launch {}: {err}", spec.program.display()))?;
    let tree = match platform::Tree::attach(&child) {
        Ok(tree) => Arc::new(tree),
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };

    if let Some(capture) = capture {
        if let Some(stdout) = child.stdout.take() {
            logging::capture_stream(
                stdout,
                format!("{} stdout", capture.label),
                capture.sink.clone(),
                capture.observer.clone(),
            );
        }
        if let Some(stderr) = child.stderr.take() {
            logging::capture_stream(
                stderr,
                format!("{} stderr", capture.label),
                capture.sink,
                capture.observer,
            );
        }
    }

    let pid = child.id();
    let exit = Arc::new(ExitCell::default());
    {
        let tree = Arc::clone(&tree);
        let exit = Arc::clone(&exit);
        std::thread::Builder::new()
            .name(format!("supervisor-{pid}"))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => status.code(),
                    Err(err) => {
                        log::warn!("cannot wait for pid {pid}: {err}");
                        None
                    }
                };
                tree.kill();
                exit.set(Exit { code });
            })
            .map_err(|err| format!("cannot supervise pid {pid}: {err}"))?;
    }

    Ok(Supervised { pid, tree, exit })
}

#[cfg(test)]
mod tests;
