use crate::protocol::{self, Change, Entry, Request, Response, Scope, MAX_LINE};
use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    reader: BufReader<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
}

impl Client {
    pub fn over(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    #[cfg(unix)]
    pub fn connect(socket: &std::path::Path) -> Result<Self, String> {
        let stream = std::os::unix::net::UnixStream::connect(socket)
            .map_err(|err| format!("cannot reach the helper: {err}"))?;
        let prepared = stream
            .set_read_timeout(Some(TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
            .and_then(|()| stream.try_clone());
        let writer = prepared.map_err(|err| format!("cannot talk to the helper: {err}"))?;
        Ok(Self::over(Box::new(stream), Box::new(writer)))
    }

    #[cfg(windows)]
    pub fn connect(pipe: &std::path::Path) -> Result<Self, String> {
        Self::connect_checked(pipe, |_| Ok(()))
    }

    #[cfg(windows)]
    pub fn connect_checked(
        pipe: &std::path::Path,
        check: impl Fn(u32) -> Result<(), String>,
    ) -> Result<Self, String> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
        use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
        use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
        let deadline = std::time::Instant::now() + TIMEOUT;
        let stream = loop {
            let opened = std::fs::OpenOptions::new()
                .access_mode(crate::server::CLIENT_ACCESS)
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(pipe);
            match opened {
                Ok(stream) => break stream,
                Err(err)
                    if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => return Err(format!("cannot reach the helper: {err}")),
            }
        };
        let mut server = 0;
        if unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut server) } == 0 {
            return Err(crate::win::last_error(
                "cannot tell which program answers the helper's pipe",
            ));
        }
        check(server)?;
        let writer = stream
            .try_clone()
            .map_err(|err| format!("cannot talk to the helper: {err}"))?;
        Ok(Self::over(Box::new(stream), Box::new(writer)))
    }

    pub fn call(&mut self, request: &Request) -> Result<Response, String> {
        let fail = |err: std::io::Error| format!("the helper did not answer: {err}");
        self.writer
            .write_all(format!("{}\n", protocol::encode(request)).as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(fail)?;
        let mut line = String::new();
        (&mut self.reader)
            .take(MAX_LINE as u64)
            .read_line(&mut line)
            .map_err(fail)?;
        if line.is_empty() {
            return Err("the helper closed the connection".to_string());
        }
        match serde_json::from_str(&line) {
            Ok(Response::Error { message }) => Err(message),
            Ok(response) => Ok(response),
            Err(err) => Err(format!("the helper sent an unreadable answer: {err}")),
        }
    }

    fn unexpected<T>(response: Response) -> Result<T, String> {
        Err(format!(
            "the helper sent an unexpected answer: {response:?}"
        ))
    }

    pub fn hello(&mut self) -> Result<u32, String> {
        match self.call(&Request::Hello {})? {
            Response::Hello { version } => Ok(version),
            other => Self::unexpected(other),
        }
    }

    pub fn apply(&mut self, owner: &str, scope: Scope, change: &Change) -> Result<String, String> {
        let request = Request::Apply {
            owner: owner.to_string(),
            scope,
            change: change.clone(),
        };
        match self.call(&request)? {
            Response::Applied { id } => Ok(id),
            other => Self::unexpected(other),
        }
    }

    fn undone(&mut self, request: &Request) -> Result<usize, String> {
        match self.call(request)? {
            Response::Done { undone } => Ok(undone),
            other => Self::unexpected(other),
        }
    }

    pub fn release(&mut self) -> Result<usize, String> {
        self.undone(&Request::Release {})
    }

    pub fn forget(&mut self, owner: &str) -> Result<usize, String> {
        self.undone(&Request::Forget {
            owner: owner.to_string(),
        })
    }

    pub fn sweep(&mut self) -> Result<usize, String> {
        self.undone(&Request::Sweep {})
    }

    pub fn status(&mut self) -> Result<Vec<Entry>, String> {
        match self.call(&Request::Status {})? {
            Response::Status { entries } => Ok(entries),
            other => Self::unexpected(other),
        }
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use crate::engine::tests::{hosts, Scratch, BASE, NAME, OWNER};
    use crate::server::{Config, Server};
    use std::net::Ipv4Addr;
    use std::path::PathBuf;
    use std::time::Instant;

    #[cfg(unix)]
    fn endpoint(scratch: &Scratch) -> PathBuf {
        scratch.0.join("helper.sock")
    }

    #[cfg(windows)]
    fn endpoint(scratch: &Scratch) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\{}",
            scratch.0.file_name().unwrap().to_string_lossy()
        ))
    }

    fn serve(scratch: &Scratch) -> PathBuf {
        let socket = endpoint(scratch);
        let server = Server::bind(Config {
            endpoint: socket.clone(),
            state_dir: scratch.0.clone(),
            hosts: scratch.hosts(),
            user: None,
            flush: false,
            programs: None,
            anchors: None,
        })
        .unwrap();
        std::thread::spawn(move || server.run());
        socket
    }

    fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn applies_and_releases_over_the_socket() {
        let scratch = Scratch::new("socket");
        let socket = serve(&scratch);
        let mut client = Client::connect(&socket).unwrap();
        assert_eq!(client.hello().unwrap(), protocol::VERSION);
        let id = client
            .apply(OWNER, Scope::Session, &hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        assert!(scratch.hosts_text().contains(NAME));
        assert_eq!(client.status().unwrap()[0].id, id);
        assert_eq!(client.release().unwrap(), 1);
        assert_eq!(scratch.hosts_text(), BASE);
        assert!(client.status().unwrap().is_empty());
        let err = client
            .apply(OWNER, Scope::Session, &hosts(Ipv4Addr::new(203, 0, 113, 7)))
            .unwrap_err();
        assert!(err.contains("not this machine"), "{err}");
        assert_eq!(scratch.hosts_text(), BASE);
    }

    #[test]
    fn a_dropped_connection_undoes_its_session_changes() {
        let scratch = Scratch::new("dropped");
        let socket = serve(&scratch);
        let mut crashed = Client::connect(&socket).unwrap();
        let mut alive = Client::connect(&socket).unwrap();
        crashed
            .apply(OWNER, Scope::Session, &hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        let kept = alive
            .apply(OWNER, Scope::Session, &hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        drop(crashed);
        eventually("the dropped lease to be undone", || {
            alive.status().unwrap().len() == 1
        });
        assert_eq!(alive.status().unwrap()[0].id, kept);
        assert!(scratch.hosts_text().contains(NAME));
        assert_eq!(alive.sweep().unwrap(), 0);
        assert!(scratch.hosts_text().contains(NAME));
        drop(alive);
        eventually("the hosts file to be clean", || {
            scratch.hosts_text() == BASE
        });
    }

    #[test]
    fn forgetting_an_owner_undoes_everything_it_holds() {
        let scratch = Scratch::new("forget");
        let socket = serve(&scratch);
        let mut session = Client::connect(&socket).unwrap();
        session
            .apply(OWNER, Scope::Session, &hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        let mut other = Client::connect(&socket).unwrap();
        assert_eq!(other.forget("0123456789abcdef/other").unwrap(), 0);
        assert!(scratch.hosts_text().contains(NAME));
        assert_eq!(other.forget(OWNER).unwrap(), 1);
        assert_eq!(scratch.hosts_text(), BASE);
        assert_eq!(session.release().unwrap(), 0);
    }

    #[test]
    fn a_restarted_helper_undoes_what_the_last_one_left() {
        let scratch = Scratch::new("restart");
        scratch
            .engine()
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        assert!(scratch.hosts_text().contains(NAME));
        let socket = serve(&scratch);
        assert_eq!(scratch.hosts_text(), BASE);
        assert!(Client::connect(&socket)
            .unwrap()
            .status()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn answers_nonsense_with_an_error() {
        let scratch = Scratch::new("nonsense");
        let socket = serve(&scratch);
        let mut client = Client::connect(&socket).unwrap();
        client
            .writer
            .write_all(b"{\"type\":\"shell\",\"run\":\"id\"}\n")
            .unwrap();
        let mut line = String::new();
        client.reader.read_line(&mut line).unwrap();
        assert!(line.contains("\"error\""), "{line}");
        assert_eq!(client.hello().unwrap(), protocol::VERSION);
    }

    #[cfg(windows)]
    #[test]
    fn the_pipe_admits_only_system_and_its_user() {
        let scratch = Scratch::new("pipe-acl");
        let pipe = serve(&scratch);
        let me = crate::win::current_user().unwrap();
        let client = Client::connect(&pipe).unwrap();
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match std::fs::OpenOptions::new().read(true).open(&pipe) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("cannot open {}: {err}", pipe.display()),
            }
        };
        let access = crate::win::access_of(&stream).unwrap();
        let expected = crate::win::Descriptor::parse(&format!(
            "D:P(A;;FA;;;SY)(A;;FA;;;{me})(A;;0x12008b;;;{me})"
        ))
        .unwrap()
        .access()
        .unwrap();
        assert_eq!(access, expected);
        assert_eq!(
            crate::server::access_list(crate::win::SYSTEM, &me),
            format!("D:P(A;;FA;;;S-1-5-18)(A;;0x12008b;;;{me})")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_second_server_cannot_take_the_pipe_and_a_wrong_server_is_refused() {
        let scratch = Scratch::new("pipe-squat");
        let pipe = serve(&scratch);
        let other = Scratch::new("pipe-squat-other");
        let squatter = Server::bind(Config {
            endpoint: pipe.clone(),
            state_dir: other.0.clone(),
            hosts: other.hosts(),
            user: None,
            flush: false,
            programs: None,
            anchors: None,
        });
        assert!(squatter.is_err());
        let me = std::process::id();
        let err = Client::connect_checked(&pipe, |server| {
            Err(format!("the pipe belongs to process {server}"))
        })
        .err()
        .unwrap();
        assert_eq!(err, format!("the pipe belongs to process {me}"));
        let mut client = Client::connect_checked(&pipe, |server| {
            if server == me {
                Ok(())
            } else {
                Err("wrong server".to_string())
            }
        })
        .unwrap();
        assert_eq!(client.hello().unwrap(), protocol::VERSION);
    }
}
