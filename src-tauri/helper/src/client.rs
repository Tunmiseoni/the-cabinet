use crate::protocol::{self, Change, Entry, Request, Response, Scope, MAX_LINE};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(socket: &Path) -> Result<Self, String> {
        let stream =
            UnixStream::connect(socket).map_err(|err| format!("cannot reach the helper: {err}"))?;
        let prepared = stream
            .set_read_timeout(Some(TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
            .and_then(|()| stream.try_clone());
        let writer = prepared.map_err(|err| format!("cannot talk to the helper: {err}"))?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    pub fn call(&mut self, request: &Request) -> Result<Response, String> {
        let fail = |err: std::io::Error| format!("the helper did not answer: {err}");
        self.writer
            .write_all(format!("{}\n", protocol::encode(request)).as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::{hosts, Scratch, BASE, NAME, OWNER};
    use crate::server::{Config, Server};
    use std::net::Ipv4Addr;
    use std::path::PathBuf;
    use std::time::Instant;

    fn serve(scratch: &Scratch) -> PathBuf {
        let socket = scratch.0.join("helper.sock");
        let server = Server::bind(Config {
            socket: socket.clone(),
            state_dir: scratch.0.clone(),
            hosts: scratch.hosts(),
            uid: None,
            flush: false,
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
}
