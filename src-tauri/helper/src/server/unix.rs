use super::{Config, Shared};
use std::io::BufReader;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

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

impl Shared {
    fn serve(&self, stream: UnixStream) -> std::io::Result<()> {
        let uid = peer_uid(&stream)?;
        if self.user.is_some_and(|allowed| uid != allowed && uid != 0) {
            log::warn!("refusing a connection from uid {uid}");
            return Ok(());
        }
        let writer = stream.try_clone()?;
        self.converse(BufReader::new(stream), writer)
    }
}

impl Server {
    pub fn bind(config: Config) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&config.state_dir)
            .map_err(|err| format!("cannot create {}: {err}", config.state_dir.display()))?;
        let _ = std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700));
        let endpoint = config.endpoint.clone();
        let shared = Shared::open(config)?;
        let _ = std::fs::remove_file(&endpoint);
        let listener = UnixListener::bind(&endpoint)
            .and_then(|listener| {
                std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))?;
                Ok(listener)
            })
            .map_err(|err| format!("cannot listen on {}: {err}", endpoint.display()))?;
        if let Some(uid) = shared.user {
            std::os::unix::fs::chown(&endpoint, Some(uid), None)
                .map_err(|err| format!("cannot hand {} to uid {uid}: {err}", endpoint.display()))?;
        }
        Ok(Self {
            listener,
            shared: Arc::new(shared),
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
            let Some(admitted) = self.shared.admit() else {
                continue;
            };
            std::thread::spawn(move || {
                if let Err(err) = admitted.0.serve(stream) {
                    log::warn!("a connection failed: {err}");
                }
            });
        }
    }
}
