use super::room::Room;
use crate::constants;
use crate::sync::MutexExt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const ROOM_PATH: &str = "/room";

pub(crate) type Handler = Arc<dyn Fn(Request) + Send + Sync>;

pub(crate) fn bind_addresses(tailnet: Option<Ipv4Addr>) -> Vec<IpAddr> {
    let mut addresses = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    if let Some(address) = tailnet.filter(|address| !address.is_loopback()) {
        addresses.push(IpAddr::V4(address));
    }
    addresses
}

pub(crate) struct Beacon {
    servers: Vec<Arc<Server>>,
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Beacon {
    pub(crate) fn start(
        addresses: &[IpAddr],
        port: u16,
        room: Arc<Mutex<Room>>,
    ) -> crate::error::Result<Self> {
        Self::serve(
            addresses,
            port,
            Arc::new(move |request| respond(request, &room)),
        )
    }

    pub(crate) fn serve(
        addresses: &[IpAddr],
        port: u16,
        handler: Handler,
    ) -> crate::error::Result<Self> {
        let beacon = Self {
            servers: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
            threads: Mutex::new(Vec::new()),
        };
        addresses.iter().try_fold(beacon, |mut beacon, address| {
            beacon.listen(SocketAddr::new(*address, port), Arc::clone(&handler))?;
            Ok(beacon)
        })
    }

    #[cfg(test)]
    fn start_on(address: SocketAddr, room: Arc<Mutex<Room>>) -> crate::error::Result<Self> {
        Self::start(&[address.ip()], address.port(), room)
    }

    fn listen(&mut self, address: SocketAddr, handler: Handler) -> crate::error::Result<()> {
        let server = Server::http(address)
            .map_err(|err| format!("cannot bind the room beacon on {address}: {err}"))?;
        let server = Arc::new(server);
        let thread = {
            let server = Arc::clone(&server);
            let stop = Arc::clone(&self.stop);
            std::thread::spawn(move || serve(&server, &stop, &handler))
        };
        self.servers.push(server);
        self.threads.lock_or_recover().push(thread);
        Ok(())
    }

    pub(crate) fn local_addresses(&self) -> Vec<SocketAddr> {
        self.servers
            .iter()
            .filter_map(|server| server.server_addr().to_ip())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn local_port(&self) -> Option<u16> {
        self.local_addresses().first().map(SocketAddr::port)
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.lock_or_recover().drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for Beacon {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(server: &Server, stop: &AtomicBool, handler: &Handler) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match server.recv_timeout(constants::LOBBY_BEACON_TICK) {
            Ok(Some(request)) => {
                let handler = Arc::clone(handler);
                let spawned = std::thread::Builder::new()
                    .name("beacon-request".to_string())
                    .spawn(move || handler(request));
                if let Err(err) = spawned {
                    log::warn!("cannot handle a beacon request: {err}");
                }
            }
            Ok(None) => {}
            Err(err) => {
                log::warn!("beacon stopped: {err}");
                break;
            }
        }
    }
}

fn respond(request: Request, room: &Arc<Mutex<Room>>) {
    let path = request.url().split('?').next().unwrap_or("");
    if request.method() == &Method::Get && path == ROOM_PATH {
        let body = serde_json::to_vec(&*room.lock_or_recover()).unwrap_or_else(|_| b"{}".to_vec());
        let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .expect("static header");
        let response = Response::from_data(body)
            .with_status_code(200)
            .with_header(header);
        let _ = request.respond(response);
    } else {
        let _ = request.respond(Response::from_string("not found").with_status_code(404));
    }
}

pub(crate) fn query(host: &str, port: u16, timeout: Duration) -> Option<Room> {
    let url = format!("http://{host}:{port}{ROOM_PATH}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut response = agent.get(&url).call().ok()?;
    let text = response.body_mut().read_to_string().ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beacon_with_room(players: u8, spectators: u8) -> (Arc<Mutex<Room>>, Beacon) {
        let room = Arc::new(Mutex::new(Room::new(
            "r1",
            "node-1",
            "player-one",
            "sfiii3nr1",
            2,
            None,
        )));
        let mut seats = crate::lobby::sets::Seats::default();
        for slot in 1..=players.min(2) {
            seats.set(slot, Some(format!("player-{slot}")));
        }
        for index in 0..spectators {
            seats.enqueue(format!("watcher-{index}"));
        }
        room.lock_or_recover().set_seats(&seats);
        let beacon = Beacon::start_on((Ipv4Addr::LOCALHOST, 0).into(), Arc::clone(&room)).unwrap();
        (room, beacon)
    }

    #[test]
    fn serves_the_room_as_json() {
        let (_room, beacon) = beacon_with_room(2, 1);
        let port = beacon.local_port().unwrap();
        let fetched = query("127.0.0.1", port, constants::LOBBY_BEACON_QUERY_TIMEOUT).unwrap();
        assert_eq!(fetched.rom, "sfiii3nr1");
        assert_eq!(fetched.host_handle, "player-one");
        assert_eq!(fetched.first_to, 2);
        assert_eq!(fetched.players, 2);
        assert_eq!(fetched.spectators, 1);
        assert_eq!(fetched.phase, crate::lobby::room::RoomPhase::Playing);
    }

    #[test]
    fn reflects_updates_to_the_shared_room() {
        let (room, beacon) = beacon_with_room(0, 0);
        let port = beacon.local_port().unwrap();
        let mut seats = crate::lobby::sets::Seats::default();
        seats.set(1, Some("player-one".into()));
        seats.set(2, Some("player-two".into()));
        for index in 0..3 {
            seats.enqueue(format!("watcher-{index}"));
        }
        room.lock_or_recover().set_seats(&seats);
        let fetched = query("127.0.0.1", port, constants::LOBBY_BEACON_QUERY_TIMEOUT).unwrap();
        assert_eq!(fetched.players, 2);
        assert_eq!(fetched.spectators, 3);
    }

    #[test]
    fn answers_none_when_nothing_is_hosting() {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        assert!(query("127.0.0.1", port, Duration::from_millis(200)).is_none());
    }

    #[test]
    fn binds_loopback_and_the_tailnet_address() {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let tailnet = Ipv4Addr::new(100, 64, 0, 1);
        assert_eq!(bind_addresses(None), vec![loopback]);
        assert_eq!(
            bind_addresses(Some(tailnet)),
            vec![loopback, IpAddr::V4(tailnet)]
        );
        assert_eq!(bind_addresses(Some(Ipv4Addr::LOCALHOST)), vec![loopback]);
    }

    #[test]
    fn a_failed_bind_releases_the_listeners_already_started() {
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let room = Arc::new(Mutex::new(Room::new(
            "r1",
            "node-1",
            "player-one",
            "sfiii3nr1",
            2,
            None,
        )));
        let addresses = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)),
        ];
        let err = Beacon::start(&addresses, port, Arc::clone(&room))
            .err()
            .unwrap();
        assert!(err.to_string().contains("203.0.113.9"), "got {err}");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let beacon = loop {
            match Beacon::start(&addresses[..1], port, Arc::clone(&room)) {
                Ok(beacon) => break beacon,
                Err(err) if std::time::Instant::now() >= deadline => {
                    panic!("port not released: {err}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        assert_eq!(beacon.local_port(), Some(port));
    }

    #[test]
    fn stops_serving_after_stop() {
        let (_room, beacon) = beacon_with_room(2, 0);
        let port = beacon.local_port().unwrap();
        assert!(query("127.0.0.1", port, constants::LOBBY_BEACON_QUERY_TIMEOUT).is_some());
        beacon.stop();
        assert!(query("127.0.0.1", port, constants::LOBBY_BEACON_QUERY_TIMEOUT).is_none());
    }
}
