use crate::engine::Engine;
use crate::protocol::{self, Request, Response, Scope, MAX_LINE, VERSION};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_CONNECTIONS: usize = 32;

pub struct Config {
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    pub hosts: PathBuf,
    pub uid: Option<u32>,
    pub flush: bool,
}

struct State {
    engine: Engine,
    live: BTreeSet<String>,
}

struct Shared {
    state: Mutex<State>,
    connections: AtomicUsize,
    uid: Option<u32>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

pub struct Server {
    listener: UnixListener,
    shared: Arc<Shared>,
}

fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let fd = stream.as_raw_fd();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut libc::ucred as *mut libc::c_void,
                &mut len,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid)
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

    fn converse(&self, stream: UnixStream) -> std::io::Result<()> {
        let uid = peer_uid(&stream)?;
        if self.uid.is_some_and(|allowed| uid != allowed && uid != 0) {
            log::warn!("refusing a connection from uid {uid}");
            return Ok(());
        }
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
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

impl Server {
    pub fn bind(config: Config) -> Result<Self, String> {
        std::fs::create_dir_all(&config.state_dir)
            .map_err(|err| format!("cannot create {}: {err}", config.state_dir.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700));
        }
        let mut engine = Engine::open(&config.state_dir, &config.hosts, config.flush)?;
        let (undone, failures) = engine.sweep(|_| false);
        if undone > 0 || !failures.is_empty() {
            log::info!("startup sweep: {undone} undone, {} failed", failures.len());
        }
        let _ = std::fs::remove_file(&config.socket);
        let listener = UnixListener::bind(&config.socket)
            .and_then(|listener| {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600))?;
                Ok(listener)
            })
            .map_err(|err| format!("cannot listen on {}: {err}", config.socket.display()))?;
        if let Some(uid) = config.uid {
            std::os::unix::fs::chown(&config.socket, Some(uid), None).map_err(|err| {
                format!(
                    "cannot hand {} to uid {uid}: {err}",
                    config.socket.display()
                )
            })?;
        }
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    engine,
                    live: BTreeSet::new(),
                }),
                connections: AtomicUsize::new(0),
                uid: config.uid,
            }),
        })
    }

    pub fn run(self) {
        for stream in self.listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(err) => {
                    log::warn!("cannot accept a connection: {err}");
                    continue;
                }
            };
            if self.shared.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                self.shared.connections.fetch_sub(1, Ordering::SeqCst);
                log::warn!("too many connections; dropping one");
                continue;
            }
            let shared = Arc::clone(&self.shared);
            std::thread::spawn(move || {
                if let Err(err) = shared.converse(stream) {
                    log::warn!("a connection failed: {err}");
                }
                shared.connections.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
}
