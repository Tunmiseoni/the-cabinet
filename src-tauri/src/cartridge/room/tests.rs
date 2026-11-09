use super::*;
use crate::lobby::beacon::Beacon;
use serde_json::json;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Hooks {
    changes: AtomicUsize,
    refuse: Option<String>,
    trusted: bool,
}

impl RoomHooks for Hooks {
    fn changed(&self, _room: &Room) {
        self.changes.fetch_add(1, Ordering::SeqCst);
    }

    fn ask_join(&self, _member: &RoomMember) -> Result<(), String> {
        match &self.refuse {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        }
    }

    fn verify(&self, _member_id: &str, _address: IpAddr) -> bool {
        self.trusted
    }
}

fn room(late_join: LateJoin, max_members: u32) -> Room {
    Room {
        protocol: PROTOCOL,
        room_id: "r1".to_string(),
        host: "nHOST".to_string(),
        cartridge: RoomCartridge {
            publisher: "unsigned".to_string(),
            id: "test".to_string(),
            version: Version::new(0, 1, 0),
            digest: "d1".to_string(),
            title: "Test".to_string(),
        },
        max_members,
        late_join,
        phase: Phase::Gathering,
        service: None,
        members: vec![RoomMember {
            id: "nHOST".to_string(),
            name: "player-one".to_string(),
            address: "100.64.0.1".to_string(),
            status: Status::Starting,
            message: None,
            public: Value::Null,
        }],
        message: None,
        revision: 0,
    }
}

fn joiner(id: &str, digest: &str) -> JoinRequest {
    JoinRequest {
        member: Joiner {
            id: id.to_string(),
            name: format!("{id}-name"),
        },
        digest: digest.to_string(),
    }
}

fn loopback() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn tailnet(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(100, 64, 0, last))
}

fn host(room: Room, hooks: Hooks) -> (Arc<RoomHost>, Arc<Hooks>) {
    let hooks = Arc::new(hooks);
    (Arc::new(RoomHost::new(room, hooks.clone())), hooks)
}

#[test]
fn joining_enforces_the_build_capacity_and_late_rules() {
    let (host, hooks) = host(
        room(LateJoin::Never, 2),
        Hooks {
            trusted: true,
            ..Hooks::default()
        },
    );
    assert_eq!(
        host.join(&joiner("n2", "other"), loopback()),
        Err(JoinError::Mismatch)
    );
    let joined = host.join(&joiner("n2", "d1"), tailnet(2)).unwrap();
    assert_eq!(joined.members.len(), 2);
    assert_eq!(joined.member("n2").unwrap().address, "100.64.0.2");
    assert_eq!(joined.revision, 1);
    assert_eq!(hooks.changes.load(Ordering::SeqCst), 1);
    assert_eq!(
        host.join(&joiner("n3", "d1"), loopback()),
        Err(JoinError::Full)
    );
    let rejoined = host.join(&joiner("n2", "d1"), tailnet(3)).unwrap();
    assert_eq!(rejoined.members.len(), 2);
    assert_eq!(rejoined.member("n2").unwrap().address, "100.64.0.3");

    host.leave("n2");
    host.update(|room| room.phase = Phase::Running);
    assert_eq!(
        host.join(&joiner("n3", "d1"), loopback()),
        Err(JoinError::Late)
    );
    host.leave("nHOST");
    assert!(host.snapshot().member("nHOST").is_some());
    host.close(Some("bye".to_string()));
    assert_eq!(
        host.join(&joiner("n3", "d1"), loopback()),
        Err(JoinError::Closed)
    );
}

#[test]
fn a_late_join_can_be_asked_and_refused() {
    let (host, _) = host(
        room(LateJoin::Ask, 8),
        Hooks {
            refuse: Some("mid-set, wait for the next one".to_string()),
            ..Hooks::default()
        },
    );
    assert!(host.join(&joiner("n2", "d1"), loopback()).is_ok());
    host.update(|room| room.phase = Phase::Running);
    assert_eq!(
        host.join(&joiner("n3", "d1"), loopback()),
        Err(JoinError::Refused(
            "mid-set, wait for the next one".to_string()
        ))
    );
    assert!(host.join(&joiner("n2", "d1"), loopback()).is_ok());
}

#[test]
fn a_remote_member_must_match_its_network_identity() {
    let (host, _) = host(room(LateJoin::Always, 8), Hooks::default());
    assert_eq!(
        host.join(&joiner("n2", "d1"), tailnet(2)),
        Err(JoinError::Unverified)
    );
    assert!(host.join(&joiner("n2", "d1"), loopback()).is_ok());
    assert!(host.owns("n2", loopback()));
    assert!(!host.owns("n2", tailnet(9)));
    assert!(!host.owns("n9", loopback()));
}

#[test]
fn unchanged_updates_do_not_bump_the_revision() {
    let (host, hooks) = host(room(LateJoin::Always, 8), Hooks::default());
    host.update(|_| ());
    assert_eq!(host.snapshot().revision, 0);
    let update = StateUpdate {
        member_id: "nHOST".to_string(),
        status: Some(Status::Ready),
        message: None,
        public: Some(json!({"seat": 1})),
    };
    assert!(host.apply(&update));
    assert!(host.apply(&update));
    assert_eq!(host.snapshot().revision, 1);
    assert_eq!(hooks.changes.load(Ordering::SeqCst), 1);
    assert!(!host.apply(&StateUpdate {
        member_id: "nobody".to_string(),
        status: None,
        message: None,
        public: None,
    }));
}

#[test]
fn wait_newer_returns_on_change_close_or_timeout() {
    let (host, _) = host(room(LateJoin::Always, 8), Hooks::default());
    let started = Instant::now();
    assert_eq!(host.wait_newer(0, Duration::from_millis(50)).revision, 0);
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert_eq!(host.wait_newer(5, Duration::from_secs(5)).revision, 0);

    let waiter = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.wait_newer(0, Duration::from_secs(10)))
    };
    std::thread::sleep(Duration::from_millis(50));
    host.join(&joiner("n2", "d1"), loopback()).unwrap();
    assert_eq!(waiter.join().unwrap().revision, 1);

    let waiter = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.wait_newer(1, Duration::from_secs(10)))
    };
    std::thread::sleep(Duration::from_millis(50));
    host.close(None);
    let started = Instant::now();
    assert_eq!(waiter.join().unwrap().phase, Phase::Closed);
    assert!(started.elapsed() < Duration::from_secs(5));
}

fn serve(host: &Arc<RoomHost>) -> (Beacon, RoomClient) {
    let handler = {
        let host = Arc::clone(host);
        Arc::new(move |request| host.handle(request, "0.1.15"))
    };
    let beacon = Beacon::serve(&[loopback()], 0, handler).unwrap();
    let port = beacon.local_addresses()[0].port();
    (beacon, RoomClient::new("127.0.0.1", port))
}

#[test]
fn speaks_the_room_protocol_over_http() {
    let (host, _) = host(room(LateJoin::Always, 8), Hooks::default());
    let (beacon, client) = serve(&host);
    let hello = client.hello().unwrap();
    assert_eq!(hello.protocol, PROTOCOL);
    assert_eq!(hello.app, "0.1.15");

    let err = client.join(&joiner("n2", "other")).unwrap_err();
    assert!(err.contains("differs from the room's"), "{err}");
    let joined = client.join(&joiner("n2", "d1")).unwrap();
    assert_eq!(joined.member("n2").unwrap().address, "127.0.0.1");

    let poll = {
        let client = client.clone();
        let since = joined.revision;
        std::thread::spawn(move || client.room(Some(since)))
    };
    std::thread::sleep(Duration::from_millis(100));
    let updated = client
        .update(&StateUpdate {
            member_id: "n2".to_string(),
            status: Some(Status::Ready),
            message: None,
            public: Some(json!({"hello": "n2"})),
        })
        .unwrap();
    let polled = poll.join().unwrap().unwrap();
    assert_eq!(polled.revision, updated.revision);
    assert_eq!(polled.member("n2").unwrap().public, json!({"hello": "n2"}));
    assert_eq!(polled.member("n2").unwrap().status, Status::Ready);

    let err = client
        .update(&StateUpdate {
            member_id: "nobody".to_string(),
            status: None,
            message: None,
            public: None,
        })
        .unwrap_err();
    assert!(err.contains("not a member"), "{err}");

    let left = client.leave("n2").unwrap();
    assert!(left.member("n2").is_none());
    assert_eq!(client.room(None).unwrap().members.len(), 1);
    beacon.stop();
}

#[test]
fn an_older_or_absent_host_is_reported_plainly() {
    let room_1x = Arc::new(Mutex::new(crate::lobby::room::Room::new(
        "r1",
        "node-1",
        "player-one",
        "sfiii3nr1",
        2,
        None,
    )));
    let old = Beacon::start(&[loopback()], 0, room_1x).unwrap();
    let port = old.local_addresses()[0].port();
    let err = RoomClient::new("127.0.0.1", port).hello().unwrap_err();
    assert!(err.contains("older Cabinet"), "{err}");
    old.stop();

    let free = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err = RoomClient::new("127.0.0.1", free).hello().unwrap_err();
    assert!(err.contains("cannot reach the host"), "{err}");
}

#[test]
fn a_1x_client_is_told_to_update() {
    let (host, _) = host(room(LateJoin::Always, 8), Hooks::default());
    let (beacon, _client) = serve(&host);
    let port = beacon.local_addresses()[0].port();
    assert!(crate::lobby::beacon::query("127.0.0.1", port, Duration::from_secs(2)).is_none());
    beacon.stop();
}
