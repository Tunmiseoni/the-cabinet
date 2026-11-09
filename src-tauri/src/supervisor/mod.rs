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
use std::future::Future;
use std::io::Write;
use std::process::{ChildStdin, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct Capture {
    pub label: String,
    pub sink: Arc<Mutex<File>>,
    pub observer: Option<LineObserver>,
    pub stdout: Option<LineObserver>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdin {
    Inherit,
    Piped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
}

#[derive(Default)]
struct ExitCell {
    value: Mutex<Option<Exit>>,
    ready: Condvar,
    notify: tokio::sync::Notify,
}

impl ExitCell {
    fn set(&self, exit: Exit) {
        *self.value.lock_or_recover() = Some(exit);
        self.ready.notify_all();
        self.notify.notify_waiters();
    }

    fn get(&self) -> Option<Exit> {
        *self.value.lock_or_recover()
    }

    async fn wait(&self) -> Exit {
        loop {
            let mut notified = std::pin::pin!(self.notify.notified());
            notified.as_mut().enable();
            if let Some(exit) = self.get() {
                return exit;
            }
            notified.await;
        }
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
    stdin: Mutex<Option<mpsc::Sender<String>>>,
}

impl Supervised {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn exited(&self) -> impl Future<Output = Exit> + Send + 'static {
        let exit = Arc::clone(&self.exit);
        async move { exit.wait().await }
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

    pub fn send_line(&self, line: &str) -> bool {
        match &*self.stdin.lock_or_recover() {
            Some(sender) => sender.send(line.to_string()).is_ok(),
            None => false,
        }
    }

    pub fn close_stdin(&self) {
        self.stdin.lock_or_recover().take();
    }

    pub async fn stop(&self, grace: Duration, farewell: Option<&str>) -> Option<Exit> {
        if let Some(exit) = self.exit.get() {
            return Some(exit);
        }
        if let Some(line) = farewell {
            self.send_line(line);
        }
        if !grace.is_zero() {
            if let Ok(exit) = tokio::time::timeout(grace, self.exited()).await {
                return Some(exit);
            }
            self.tree.terminate();
            if let Ok(exit) =
                tokio::time::timeout(crate::constants::SUPERVISOR_TERM_GRACE, self.exited()).await
            {
                return Some(exit);
            }
        }
        self.tree.kill();
        tokio::time::timeout(crate::constants::SUPERVISOR_REAP_TIMEOUT, self.exited())
            .await
            .ok()
    }
}

fn feed_stdin(pid: u32, mut stdin: ChildStdin, lines: mpsc::Receiver<String>) {
    for line in lines {
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush());
        if let Err(err) = written {
            log::debug!("stdin of pid {pid} closed: {err}");
            break;
        }
    }
}

fn wait_for_streams(streams: Vec<JoinHandle<()>>) {
    let deadline = Instant::now() + crate::constants::SUPERVISOR_REAP_TIMEOUT;
    while streams.iter().any(|stream| !stream.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn spawn(spec: &LaunchSpec, capture: Option<Capture>) -> crate::error::Result<Supervised> {
    spawn_with(spec, capture, Stdin::Inherit)
}

pub fn spawn_with(
    spec: &LaunchSpec,
    capture: Option<Capture>,
    stdin: Stdin,
) -> crate::error::Result<Supervised> {
    let mut command = crate::process::command(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(spec.envs.iter().cloned());
    if capture.is_some() {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    if stdin == Stdin::Piped {
        command.stdin(Stdio::piped());
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

    let mut streams = Vec::new();
    if let Some(capture) = capture {
        if let Some(stdout) = child.stdout.take() {
            streams.push(logging::capture_stream(
                stdout,
                format!("{} stdout", capture.label),
                capture.sink.clone(),
                capture.stdout.or_else(|| capture.observer.clone()),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            streams.push(logging::capture_stream(
                stderr,
                format!("{} stderr", capture.label),
                capture.sink,
                capture.observer,
            ));
        }
    }

    let pid = child.id();
    let stdin = child.stdin.take().and_then(|pipe| {
        let (sender, lines) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("supervisor-{pid}-stdin"))
            .spawn(move || feed_stdin(pid, pipe, lines))
            .map_err(|err| log::warn!("cannot feed stdin of pid {pid}: {err}"))
            .ok()
            .map(|_| sender)
    });
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
                wait_for_streams(streams);
                exit.set(Exit { code });
            })
            .map_err(|err| format!("cannot supervise pid {pid}: {err}"))?;
    }

    Ok(Supervised {
        pid,
        tree,
        exit,
        stdin: Mutex::new(stdin),
    })
}

#[cfg(test)]
mod tests;
