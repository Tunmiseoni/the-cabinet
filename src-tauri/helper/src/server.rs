#![cfg_attr(not(any(unix, windows)), allow(dead_code))]

use crate::engine::Engine;
use crate::protocol::{self, Request, Response, Scope, MAX_LINE, VERSION};
use std::collections::BTreeSet;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::Server;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{access_list, Server, CLIENT_ACCESS};

const MAX_CONNECTIONS: usize = 32;

#[cfg(unix)]
pub type User = u32;
#[cfg(not(unix))]
pub type User = String;

pub struct Config {
    pub endpoint: PathBuf,
    pub state_dir: PathBuf,
    pub hosts: PathBuf,
    pub user: Option<User>,
    pub flush: bool,
    pub programs: Option<crate::firewall::Programs>,
    pub anchors: Option<Box<dyn crate::anchors::Anchors>>,
}

struct State {
    engine: Engine,
    live: BTreeSet<String>,
}

pub(crate) struct Shared {
    state: Mutex<State>,
    connections: AtomicUsize,
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) user: Option<User>,
}

pub(crate) struct Admitted(pub(crate) Arc<Shared>);

impl Drop for Admitted {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn respond(result: Result<Response, String>) -> Response {
    result.unwrap_or_else(|message| Response::Error { message })
}

fn done(undone: usize, failures: Vec<String>) -> Result<Response, String> {
    if failures.is_empty() {
        Ok(Response::Done { undone })
    } else {
        Err(failures.join("; "))
    }
}

impl Shared {
    pub(crate) fn open(config: Config) -> Result<Self, String> {
        std::fs::create_dir_all(&config.state_dir)
            .map_err(|err| format!("cannot create {}: {err}", config.state_dir.display()))?;
        let mut engine = Engine::open(&config.state_dir, &config.hosts, config.flush)?
            .with_programs(config.programs)
            .with_anchors(config.anchors);
        let (undone, failures) = engine.sweep(|_| false);
        if undone > 0 || !failures.is_empty() {
            log::info!("startup sweep: {undone} undone, {} failed", failures.len());
        }
        Ok(Self {
            state: Mutex::new(State {
                engine,
                live: BTreeSet::new(),
            }),
            connections: AtomicUsize::new(0),
            user: config.user,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub(crate) fn admit(self: &Arc<Self>) -> Option<Admitted> {
        if self.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            self.connections.fetch_sub(1, Ordering::SeqCst);
            log::warn!("too many connections; dropping one");
            return None;
        }
        Some(Admitted(Arc::clone(self)))
    }

    fn release(&self, held: &mut Vec<String>) -> Result<Response, String> {
        let mut state = self.state();
        let ids = std::mem::take(held);
        for id in &ids {
            state.live.remove(id);
        }
        let (undone, failures) = state.engine.undo_where(|entry| ids.contains(&entry.id));
        done(undone, failures)
    }

    fn answer(&self, request: Request, held: &mut Vec<String>) -> Response {
        respond(match request {
            Request::Hello {} => Ok(Response::Hello { version: VERSION }),
            Request::Apply {
                owner,
                scope,
                change,
            } => {
                let mut state = self.state();
                state.engine.apply(&owner, scope, change).map(|entry| {
                    if entry.scope == Scope::Session {
                        state.live.insert(entry.id.clone());
                        held.push(entry.id.clone());
                    }
                    Response::Applied { id: entry.id }
                })
            }
            Request::Release {} => self.release(held),
            Request::Forget { owner } => {
                let mut state = self.state();
                let (undone, failures) = state.engine.undo_where(|entry| entry.owner == owner);
                let left: BTreeSet<String> = state
                    .engine
                    .entries()
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect();
                state.live.retain(|id| left.contains(id));
                done(undone, failures)
            }
            Request::Sweep {} => {
                let mut state = self.state();
                let State { engine, live } = &mut *state;
                let (undone, failures) = engine.sweep(|entry| live.contains(&entry.id));
                done(undone, failures)
            }
            Request::Status {} => Ok(Response::Status {
                entries: self.state().engine.entries().to_vec(),
            }),
        })
    }

    pub(crate) fn converse(
        &self,
        mut reader: impl BufRead,
        mut writer: impl Write,
    ) -> std::io::Result<()> {
        let mut held = Vec::new();
        let result = (|| loop {
            let mut line = String::new();
            let read = (&mut reader).take(MAX_LINE as u64).read_line(&mut line)?;
            if read == 0 || !line.ends_with('\n') {
                return Ok(());
            }
            let response = match serde_json::from_str::<Request>(&line) {
                Ok(request) => self.answer(request, &mut held),
                Err(err) => Response::Error {
                    message: format!("not a request this helper knows: {err}"),
                },
            };
            writer.write_all(format!("{}\n", protocol::encode(&response)).as_bytes())?;
            writer.flush()?;
        })();
        if !held.is_empty() {
            log::info!("a caller left without releasing; undoing {held:?}");
            if let Err(err) = self.release(&mut held) {
                log::error!("cannot undo a departed caller's changes: {err}");
            }
        }
        result
    }
}
