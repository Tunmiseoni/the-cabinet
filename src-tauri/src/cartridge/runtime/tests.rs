use super::*;
use crate::cartridge::assets::tests::FakeFetch;
use crate::cartridge::install::Installer;
use crate::cartridge::privileged::SharedHelper;
use crate::cartridge::testing::{build_cart, CartSpec, Scratch};
use crate::cartridge::trust::{Approval, Grant, Standing};
use crate::cartridge::{echo, store::Installed};
use cabinet_cartridge::archive;
use cabinet_cartridge::signature::KeySigner;
use cabinet_cartridge::{Publisher, Version};
use serde_json::json;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

struct Bench {
    scratch: Scratch,
    store: Arc<Mutex<Store>>,
    installed: Installed,
    helper: Option<SharedHelper>,
}

fn platform() -> Platform {
    Platform::current().unwrap()
}

fn bench(tag: &str, extra: &str) -> Bench {
    echo::install_env();
    let scratch = Scratch::new(tag);
    let cart = build_cart(
        &scratch,
        &CartSpec {
            extra,
            unix: echo::UNIX,
            windows: echo::WINDOWS,
            ..CartSpec::default()
        },
    );
    let mut store = Store::open(scratch.path("app")).unwrap();
    let installed = Installer {
        store: &mut store,
        cabinet: Version::new(0, 1, 15),
        platform: platform(),
        fetch: &FakeFetch::default(),
        log_dir: scratch.path("logs"),
        now: 1,
        busy: false,
        trust: None,
    }
    .install(&cart, None, &Approval::default())
    .unwrap();
    Bench {
        scratch,
        store: Arc::new(Mutex::new(store)),
        installed,
        helper: None,
    }
}

fn me(id: &str) -> Me {
    Me {
        id: id.to_string(),
        name: format!("{id}-name"),
    }
}

fn quiet() -> Observer {
    Arc::new(|_| {})
}

impl Bench {
    fn prepared(&self) -> Prepared {
        Prepared::from_store(
            &self.store.lock_or_recover(),
            &self.installed.cartridge,
            None,
            platform(),
        )
        .unwrap()
    }

    fn host(&self, id: &str) -> Session {
        Session::host(
            self.prepared(),
            me(id),
            HostOptions {
                bind: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
                port: 0,
                address: "127.0.0.1".to_string(),
                app_version: "0.1.15".to_string(),
                verify: Arc::new(|_, _| true),
                log_dir: self.scratch.path(&format!("logs/{id}")),
                helper: self.helper.clone(),
            },
            quiet(),
        )
        .unwrap()
    }

    fn join(&self, host: &Session, id: &str, name: &str) -> crate::error::Result<Session> {
        let store = Arc::clone(&self.store);
        Session::join(
            move |cartridge| {
                Prepared::from_store(
                    &store.lock_or_recover(),
                    &CartridgeRef {
                        publisher: cartridge.publisher.clone(),
                        id: cartridge.id.clone(),
                    },
                    Some(&cartridge.digest),
                    platform(),
                )
            },
            Me {
                id: id.to_string(),
                name: name.to_string(),
            },
            JoinOptions {
                host: "127.0.0.1".to_string(),
                port: host.addresses()[0].port(),
                log_dir: self.scratch.path(&format!("logs/{id}")),
                helper: self.helper.clone(),
            },
            quiet(),
        )
    }
}

fn eventually(what: &str, session: &Session, check: impl Fn(&SessionView) -> bool) -> SessionView {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = session.view();
        if check(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {view:#?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn public<'a>(view: &'a SessionView, id: &str) -> &'a Value {
    &view.room.member(id).expect("member in room").public
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tauri::async_runtime::block_on(future)
}

#[cfg(unix)]
fn gone(pid: &Value) -> bool {
    let pid = pid.as_i64().unwrap() as libc::pid_t;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn host_and_member_exchange_state_on_loopback_and_stop_cleanly() {
    let bench = bench("rt-pair", "");
    let host = bench.host("nHOST");
    let member = bench.join(&host, "nMEMBER", "friend").unwrap();
    let gathered = eventually("the member in the room", &host, |view| {
        view.room.members.len() == 2
    });
    assert_eq!(gathered.room.phase, Phase::Gathering);
    assert!(gathered.running.is_empty());

    block_on(host.start()).unwrap();
    assert!(block_on(host.start()).is_err());
    let view = eventually("both sides to see each other", &host, |view| {
        view.room.members.iter().all(|m| m.status == Status::Ready)
            && public(view, "nHOST")["seen"]["nMEMBER"] == json!("nMEMBER")
            && public(view, "nMEMBER")["seen"]["nHOST"] == json!("nHOST")
    });
    assert_eq!(public(&view, "nHOST")["role"], json!("host"));
    assert_eq!(public(&view, "nMEMBER")["role"], json!("member"));
    assert_eq!(public(&view, "nMEMBER")["members"], json!(2));
    assert_eq!(public(&view, "nHOST")["service"], Value::Null);
    assert!(view.ui.contains(&Widget::Text {
        value: "hello nHOST-name".to_string()
    }));
    let member_view = eventually("the member's ui", &member, |view| !view.ui.is_empty());
    assert_eq!(member_view.running, vec!["session"]);
    assert_eq!(member_view.role, Role::Member);

    member.action("bump");
    eventually("the member's action", &host, |view| {
        public(view, "nMEMBER")["actions"] == json!(["bump"])
    });
    let pids = [
        public(&view, "nHOST")["pid"].clone(),
        public(&view, "nMEMBER")["pid"].clone(),
    ];

    block_on(member.stop());
    let ended = member.view();
    assert!(ended.ended);
    assert_eq!(ended.error, None);
    assert!(ended.running.is_empty());
    eventually("the member to leave", &host, |view| {
        view.room.members.len() == 1
    });

    block_on(host.stop());
    let ended = host.view();
    assert!(ended.ended);
    assert!(ended.running.is_empty());
    assert_eq!(ended.room.phase, Phase::Closed);
    assert!(host.is_ended());
    #[cfg(unix)]
    for pid in &pids {
        assert!(gone(pid), "pid {pid} still running");
    }
    let _ = pids;
    assert!(RoomClient::new("127.0.0.1", host.addresses()[0].port())
        .hello()
        .is_err());
}

#[test]
fn the_service_starts_first_and_members_wait_for_it() {
    let bench = bench(
        "rt-service",
        "[lifecycle.service]\nprogram = \"main\"\nargs = [\"--service\"]\n",
    );
    let host = bench.host("nHOST");
    block_on(host.start()).unwrap();
    let view = eventually("the host session", &host, |view| {
        view.running == ["service", "session"] && !public(view, "nHOST").is_null()
    });
    assert_eq!(view.room.service, Some(ServiceState { ready: true }));
    assert_eq!(public(&view, "nHOST")["service"], json!("127.0.0.1"));

    let member = bench.join(&host, "nLATE", "late friend").unwrap();
    let view = eventually("the late member", &host, |view| {
        view.room
            .member("nLATE")
            .is_some_and(|m| m.status == Status::Ready && !m.public.is_null())
    });
    assert_eq!(public(&view, "nLATE")["service"], json!("127.0.0.1"));
    block_on(host.stop());
    block_on(member.ended());
    let ended = member.view();
    assert_eq!(ended.error.as_deref(), Some("the host closed the room"));
    assert!(ended.running.is_empty());
}

#[test]
fn a_late_join_is_put_to_the_hosts_game() {
    let bench = bench("rt-ask", "[room]\nlate_join = \"ask\"\n");
    let host = bench.host("nHOST");
    block_on(host.start()).unwrap();
    eventually("the host game", &host, |view| {
        view.room.member("nHOST").unwrap().status == Status::Ready
    });
    let err = bench.join(&host, "nNO", "deny me").err().unwrap();
    assert!(err.to_string().contains("no deny"), "{err}");
    let member = bench.join(&host, "nYES", "welcome").unwrap();
    eventually("the admitted member", &host, |view| {
        view.room.members.len() == 2
    });
    block_on(member.stop());
    block_on(host.stop());
}

#[test]
fn a_crashing_game_ends_the_session_for_everyone() {
    let bench = bench("rt-crash", "");
    let host = bench.host("nHOST");
    let member = bench.join(&host, "nMEMBER", "friend").unwrap();
    block_on(host.start()).unwrap();
    eventually("both ready", &host, |view| {
        view.room.members.len() == 2 && view.room.members.iter().all(|m| m.status == Status::Ready)
    });
    host.action("crash");
    block_on(host.ended());
    let ended = host.view();
    assert_eq!(ended.error.as_deref(), Some("the game exited with code 4"));
    assert_eq!(ended.room.phase, Phase::Closed);
    block_on(member.ended());
    assert_eq!(
        member.view().error.as_deref(),
        Some("the game exited with code 4")
    );
}

#[test]
fn a_game_that_ignores_stop_is_killed_after_the_grace_period() {
    let bench = bench("rt-stubborn", "");
    let host = bench.host("nHOST");
    block_on(host.start()).unwrap();
    eventually("the host game", &host, |view| {
        !public(view, "nHOST").is_null()
    });
    host.action("stubborn");
    let view = eventually("the stubborn flag", &host, |view| {
        public(view, "nHOST")["actions"] == json!(["stubborn"])
    });
    let started = Instant::now();
    block_on(host.stop());
    let took = started.elapsed();
    assert!(took >= crate::constants::CARTRIDGE_STOP_GRACE, "{took:?}");
    assert!(took < Duration::from_secs(15), "{took:?}");
    assert!(host.view().running.is_empty());
    #[cfg(unix)]
    assert!(gone(&public(&view, "nHOST")["pid"]));
    let _ = view;
}

#[test]
fn checks_and_settings_gate_hosting_and_starting() {
    let bench = bench(
        "rt-checks",
        "[[checks]]\nlabel = \"Game\"\npath = \"${data}/game.bin\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\nmessage = \"Your game.bin differs.\"\n",
    );
    let err = Session::host(
        bench.prepared(),
        me("nHOST"),
        HostOptions {
            bind: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            port: 0,
            address: "127.0.0.1".to_string(),
            app_version: "0.1.15".to_string(),
            verify: Arc::new(|_, _| true),
            log_dir: bench.scratch.path("logs"),
            helper: None,
        },
        quiet(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(err.starts_with("Game: "), "{err}");
    assert!(err.contains("was not found"), "{err}");
    let prepared = bench.prepared();
    std::fs::write(prepared.data_dir.join("game.bin"), b"other").unwrap();
    assert_eq!(
        prepared.preflight().unwrap_err().to_string(),
        "Game: Your game.bin differs."
    );
}

#[test]
fn a_member_without_the_rooms_build_is_told_to_install_it() {
    let bench = bench("rt-missing", "");
    let host = bench.host("nHOST");
    let port = host.addresses()[0].port();
    let err = Session::join(
        |cartridge| Err(format!("no {} {} here", cartridge.id, cartridge.version).into()),
        me("nMEMBER"),
        JoinOptions {
            host: "127.0.0.1".to_string(),
            port,
            log_dir: bench.scratch.path("logs"),
            helper: None,
        },
        quiet(),
    )
    .err()
    .unwrap();
    assert_eq!(err.to_string(), "no test 0.1.0 here");
    let other = CartridgeRef::new(None, "test");
    let err = Prepared::from_store(
        &bench.store.lock_or_recover(),
        &other,
        Some("feedfacefeedface"),
        platform(),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("install it first"), "{err}");
    block_on(host.stop());
}

fn echo_cart_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let name = format!("echo-cart{}", std::env::consts::EXE_SUFFIX);
    exe.ancestors()
        .skip(1)
        .map(|dir| dir.join(&name))
        .find(|binary| binary.is_file())
        .unwrap_or_else(|| {
            panic!(
                "{name} is missing above {}: build it with `cargo build -p echo-cart` (`cargo test --workspace` does)",
                exe.display()
            )
        })
}

fn test_cartridge(tag: &str, settings: Map<String, Value>) -> (Bench, Publisher) {
    test_cartridge_in(Scratch::new(tag), settings)
}

fn test_cartridge_in(scratch: Scratch, settings: Map<String, Value>) -> (Bench, Publisher) {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../cartridges/test/cartridge.toml"),
    )
    .unwrap();
    let manifest = Manifest::parse(&text, None).unwrap();
    let stage = scratch.path("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("cartridge.toml"), &text).unwrap();
    let own = format!("files/{}/", platform());
    for bundled in manifest.bundled_paths() {
        let path = stage.join(bundled);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        if bundled.starts_with(&own) {
            std::fs::copy(echo_cart_binary(), &path).unwrap();
        } else {
            std::fs::write(&path, b"another platform's build").unwrap();
        }
    }
    let (secret, _) = KeySigner::generate(None).unwrap();
    let signer = KeySigner::from_key_file(&secret, None).unwrap();
    let cart = scratch.path("test-0.1.0.cart");
    archive::pack(&stage, &cart, Some(&signer)).unwrap();
    let mut store = Store::open(scratch.path("app")).unwrap();
    let installed = Installer {
        store: &mut store,
        cabinet: Version::new(2, 0, 0),
        platform: platform(),
        fetch: &FakeFetch::default(),
        log_dir: scratch.path("logs"),
        now: 1,
        busy: false,
        trust: None,
    }
    .install(
        &cart,
        Some(settings),
        &Approval {
            trust_publisher: Some("Test publisher".to_string()),
            capabilities: true,
        },
    )
    .unwrap();
    let bench = Bench {
        scratch,
        store: Arc::new(Mutex::new(store)),
        installed,
        helper: None,
    };
    (bench, signer.publisher())
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct FakeFirewall(Arc<Mutex<Vec<String>>>);

#[cfg(unix)]
impl cabinet_helper::firewall::AppFirewall for FakeFirewall {
    fn enabled(&self) -> Result<bool, String> {
        Ok(true)
    }

    fn listed(&self, program: &str) -> Result<bool, String> {
        Ok(self.0.lock_or_recover().iter().any(|app| app == program))
    }

    fn allow(&self, program: &str) -> Result<(), String> {
        self.0.lock_or_recover().push(program.to_string());
        Ok(())
    }

    fn remove(&self, program: &str) -> Result<(), String> {
        self.0.lock_or_recover().retain(|app| app != program);
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct FakeGrant(Arc<Mutex<Vec<String>>>);

#[cfg(unix)]
impl cabinet_helper::lowports::PortGrant for FakeGrant {
    fn granted(&self, program: &str) -> Result<bool, String> {
        Ok(self.0.lock_or_recover().iter().any(|held| held == program))
    }

    fn grant(&self, program: &str) -> Result<(), String> {
        self.0.lock_or_recover().push(program.to_string());
        Ok(())
    }

    fn revoke(&self, program: &str) -> Result<(), String> {
        self.0.lock_or_recover().retain(|held| held != program);
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct FakePorts(Arc<Mutex<Vec<cabinet_helper::protocol::Port>>>);

#[cfg(unix)]
impl cabinet_helper::firewall::PortFirewall for FakePorts {
    fn enabled(&self) -> Result<bool, String> {
        Ok(true)
    }

    fn is_open(&self, port: &cabinet_helper::protocol::Port) -> Result<bool, String> {
        Ok(self.0.lock_or_recover().contains(port))
    }

    fn open(&self, port: &cabinet_helper::protocol::Port) -> Result<(), String> {
        self.0.lock_or_recover().push(port.clone());
        Ok(())
    }

    fn close(&self, port: &cabinet_helper::protocol::Port) -> Result<(), String> {
        self.0.lock_or_recover().retain(|open| open != port);
        Ok(())
    }
}

#[cfg(unix)]
struct FakeSystem {
    socket: PathBuf,
    hosts: PathBuf,
    firewall: FakeFirewall,
    ports: FakePorts,
    low_ports: FakeGrant,
}

#[cfg(unix)]
impl FakeSystem {
    const HOSTS: &'static str = "127.0.0.1\tlocalhost\n# >>> other-tool\n127.0.0.1 api.example-game.net\n# <<< other-tool\n";
    const REDIRECT: &'static str = "127.0.0.1 cabinet-test.invalid\n";

    fn start(scratch: &Scratch) -> Self {
        let dir = scratch.path("system");
        std::fs::create_dir_all(&dir).unwrap();
        let system = Self {
            socket: dir.join("helper.sock"),
            hosts: dir.join("hosts"),
            firewall: FakeFirewall::default(),
            ports: FakePorts::default(),
            low_ports: FakeGrant::default(),
        };
        std::fs::write(&system.hosts, Self::HOSTS).unwrap();
        let server = cabinet_helper::server::Server::bind(cabinet_helper::server::Config {
            endpoint: system.socket.clone(),
            state_dir: dir.join("state"),
            hosts: system.hosts.clone(),
            user: None,
            flush: false,
            programs: Some(cabinet_helper::firewall::Programs {
                firewall: cfg!(target_os = "macos").then(|| {
                    Box::new(system.firewall.clone())
                        as Box<dyn cabinet_helper::firewall::AppFirewall>
                }),
                port_firewall: cfg!(target_os = "linux").then(|| {
                    Box::new(system.ports.clone())
                        as Box<dyn cabinet_helper::firewall::PortFirewall>
                }),
                low_ports: Some(Box::new(system.low_ports.clone())),
                root: scratch.path("app"),
                uid: None,
            }),
            anchors: None,
        })
        .unwrap();
        std::thread::spawn(move || server.run());
        system
    }

    fn helper(&self) -> SharedHelper {
        Arc::new(crate::cartridge::privileged::SocketHelper {
            socket: self.socket.clone(),
            installed: None,
        })
    }

    fn hosts(&self) -> String {
        std::fs::read_to_string(&self.hosts).unwrap()
    }

    fn redirects(&self) -> usize {
        self.hosts().matches(Self::REDIRECT).count()
    }

    fn leave_a_redirect(&self, cartridge: &CartridgeRef) -> cabinet_helper::client::Client {
        let mut client = cabinet_helper::client::Client::connect(&self.socket).unwrap();
        client
            .apply(
                &crate::cartridge::privileged::owner(cartridge),
                cabinet_helper::protocol::Scope::Session,
                &cabinet_helper::protocol::Change::Hosts {
                    names: vec!["cabinet-test.invalid".to_string()],
                    target: Ipv4Addr::LOCALHOST,
                },
            )
            .unwrap();
        assert_eq!(self.redirects(), 1);
        client
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs The Cabinet's helper installed on this Mac; it edits /etc/hosts and the firewall's app list"]
fn the_installed_helper_redirects_the_test_name_for_a_session() {
    let system = FakeSystem {
        socket: cabinet_helper::system::SOCKET.into(),
        hosts: cabinet_helper::system::HOSTS.into(),
        firewall: FakeFirewall::default(),
        ports: FakePorts::default(),
        low_ports: FakeGrant::default(),
    };
    let before = system.hosts();
    assert_eq!(system.redirects(), 0, "{before}");
    let scratch_dir = Scratch::new("rt-live-helper-scratch");
    let firewall_apps = || {
        let output = std::process::Command::new("/usr/libexec/ApplicationFirewall/socketfilterfw")
            .arg("--listapps")
            .output()
            .unwrap();
        cabinet_helper::firewall::listed(&String::from_utf8_lossy(&output.stdout))
    };
    let apps_before = firewall_apps();
    let allowed = |apps: &[(String, bool)]| -> Vec<String> {
        apps.iter()
            .filter(|(path, allowed)| *allowed && path.ends_with("/echo-cart"))
            .filter(|(path, _)| path.contains("/live-test-"))
            .map(|(path, _)| path.clone())
            .collect()
    };
    let app_data = crate::env::home_dir()
        .unwrap()
        .join(cabinet_helper::system::APP_DATA);
    let (mut bench, _) = test_cartridge_in(
        Scratch::at(app_data.join(format!("live-test-{}", std::process::id()))),
        Map::from_iter([(
            "scratch_dir".to_string(),
            json!(scratch_dir.path("").to_string_lossy()),
        )]),
    );
    bench.helper = crate::cartridge::privileged::system();
    let settled = |what: &str, wanted: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while system.redirects() != wanted {
            assert!(Instant::now() < deadline, "{what}: {}", system.hosts());
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let host = bench.host("nHOST");
    let member = bench.join(&host, "nMEMBER", "friend").unwrap();
    assert_eq!(system.redirects(), 0);
    block_on(host.start()).unwrap();
    eventually("both sides ready", &host, |view| {
        view.room.members.iter().all(|m| m.status == Status::Ready)
    });
    assert_eq!(system.redirects(), 2, "{}", system.hosts());
    let during = firewall_apps();
    assert_eq!(allowed(&during).len(), 1, "{during:?}");
    assert_eq!(during.len(), apps_before.len() + 1);
    block_on(member.stop());
    assert_eq!(system.redirects(), 1, "{}", system.hosts());
    assert_eq!(allowed(&firewall_apps()).len(), 1);
    block_on(host.stop());
    assert_eq!(system.hosts(), before);
    assert_eq!(firewall_apps(), apps_before);

    let mut crashed = cabinet_helper::client::Client::connect(&system.socket).unwrap();
    crashed
        .apply(
            &crate::cartridge::privileged::owner(&bench.installed.cartridge),
            cabinet_helper::protocol::Scope::Session,
            &cabinet_helper::protocol::Change::Firewall {
                program: allowed(&during)[0].clone(),
                ports: Vec::new(),
                keep: false,
                opened: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(allowed(&firewall_apps()).len(), 1);
    drop(crashed);
    let deadline = Instant::now() + Duration::from_secs(10);
    while firewall_apps() != apps_before {
        assert!(
            Instant::now() < deadline,
            "a crashed caller's allowance stayed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut outsider = cabinet_helper::client::Client::connect(&system.socket).unwrap();
    for program in ["/bin/sh", "/usr/bin/python3"] {
        outsider
            .apply(
                &crate::cartridge::privileged::owner(&bench.installed.cartridge),
                cabinet_helper::protocol::Scope::Session,
                &cabinet_helper::protocol::Change::Firewall {
                    program: program.to_string(),
                    ports: Vec::new(),
                    keep: false,
                    opened: Vec::new(),
                },
            )
            .unwrap_err();
    }
    assert_eq!(firewall_apps(), apps_before);

    let crashed = system.leave_a_redirect(&bench.installed.cartridge);
    drop(crashed);
    settled("a crashed caller's redirect is undone", 0);
    assert_eq!(system.hosts(), before);

    let _held = system.leave_a_redirect(&bench.installed.cartridge);
    let problems = crate::cartridge::install::uninstall(
        &mut bench.store.lock_or_recover(),
        &bench.installed.cartridge,
        platform(),
        &bench.scratch.path("logs"),
        bench.helper.as_deref(),
        None,
    )
    .unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(system.hosts(), before);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs The Cabinet's helper installed on this machine; it edits /etc/hosts"]
fn the_installed_helper_redirects_the_test_name_for_a_session() {
    use std::net::ToSocketAddrs;
    let system = FakeSystem {
        socket: cabinet_helper::system::SOCKET.into(),
        hosts: cabinet_helper::system::HOSTS.into(),
        firewall: FakeFirewall::default(),
        ports: FakePorts::default(),
        low_ports: FakeGrant::default(),
    };
    let before = system.hosts();
    assert_eq!(system.redirects(), 0, "{before}");
    let resolves_to_loopback = || {
        ("cabinet-test.invalid", 0)
            .to_socket_addrs()
            .map(|mut found| found.any(|address| address.ip() == Ipv4Addr::LOCALHOST))
            .unwrap_or(false)
    };
    assert!(
        !resolves_to_loopback(),
        "the test name resolves before the session"
    );
    let low_port = || echo_cart::bind_only(987).err().map(|err| err.kind());
    assert_eq!(
        low_port(),
        Some(std::io::ErrorKind::PermissionDenied),
        "this user may already bind low ports, so the test proves nothing"
    );
    let scratch_dir = Scratch::new("rt-live-helper-scratch");
    let app_data = crate::env::home_dir()
        .unwrap()
        .join(cabinet_helper::system::APP_DATA);
    let (mut bench, _) = test_cartridge_in(
        Scratch::at(app_data.join(format!("live-test-{}", std::process::id()))),
        Map::from_iter([(
            "scratch_dir".to_string(),
            json!(scratch_dir.path("").to_string_lossy()),
        )]),
    );
    bench.helper = crate::cartridge::privileged::system();
    let settled = |what: &str, wanted: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while system.redirects() != wanted {
            assert!(Instant::now() < deadline, "{what}: {}", system.hosts());
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let host = bench.host("nHOST");
    let member = bench.join(&host, "nMEMBER", "friend").unwrap();
    assert_eq!(system.redirects(), 0);
    block_on(host.start()).unwrap();
    eventually("both sides ready", &host, |view| {
        view.room.members.iter().all(|m| m.status == Status::Ready)
    });
    assert_eq!(system.redirects(), 2, "{}", system.hosts());
    assert!(
        resolves_to_loopback(),
        "the test name does not resolve during the session"
    );
    let granted_program = cabinet_helper::client::Client::connect(&system.socket)
        .unwrap()
        .status()
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.change {
            cabinet_helper::protocol::Change::LowPorts { program, .. } => Some(program),
            _ => None,
        })
        .expect("the helper holds no low-port grant during the session");
    let carries_right = |program: &str| {
        use cabinet_helper::lowports::PortGrant;
        cabinet_helper::lowports::FileCapability.granted(program)
    };
    assert_eq!(carries_right(&granted_program), Ok(true));
    let host_logs = bench.scratch.path("logs/nHOST");
    let service_bound = || {
        std::fs::read_dir(&host_logs)
            .into_iter()
            .flatten()
            .flatten()
            .any(|log| {
                std::fs::read_to_string(log.path())
                    .is_ok_and(|text| text.contains("echo-cart bound port 987"))
            })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !service_bound() {
        assert!(
            Instant::now() < deadline,
            "the service did not bind its low port; logs in {}",
            host_logs.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let expect_ufw = std::env::var_os("CABINET_EXPECT_UFW").is_some();
    let tcp_987 = cabinet_helper::protocol::Port {
        proto: cabinet_helper::protocol::Proto::Tcp,
        port: 987,
    };
    let ufw_allows_987 = || {
        let output = std::process::Command::new("sudo")
            .args(["-n", "ufw", "status"])
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&output.stdout).into_owned();
        (
            cabinet_helper::firewall::ufw_allows(&status, &tcp_987),
            status,
        )
    };
    if expect_ufw {
        let entries = cabinet_helper::client::Client::connect(&system.socket)
            .unwrap()
            .status()
            .unwrap();
        assert!(
            entries.iter().any(|entry| matches!(&entry.change,
                cabinet_helper::protocol::Change::Firewall { opened, .. } if opened == &vec![tcp_987.clone()])),
            "{entries:?}"
        );
        let (allowed, status) = ufw_allows_987();
        assert!(allowed, "{status}");
    }
    block_on(member.stop());
    assert_eq!(system.redirects(), 1, "{}", system.hosts());
    block_on(host.stop());
    assert_eq!(system.hosts(), before);
    assert!(
        !resolves_to_loopback(),
        "the test name still resolves after the session"
    );
    assert_eq!(
        carries_right(&granted_program),
        Ok(false),
        "the service program kept its low-port right after the session"
    );
    if expect_ufw {
        let (allowed, status) = ufw_allows_987();
        assert!(!allowed, "{status}");
    }
    assert!(cabinet_helper::client::Client::connect(&system.socket)
        .unwrap()
        .status()
        .unwrap()
        .is_empty());

    let mut outsider = cabinet_helper::client::Client::connect(&system.socket).unwrap();
    outsider
        .apply(
            &crate::cartridge::privileged::owner(&bench.installed.cartridge),
            cabinet_helper::protocol::Scope::Session,
            &cabinet_helper::protocol::Change::Hosts {
                names: vec!["api.example-game.net".to_string()],
                target: Ipv4Addr::LOCALHOST,
            },
        )
        .unwrap_err();
    assert_eq!(system.hosts(), before);

    let crashed = system.leave_a_redirect(&bench.installed.cartridge);
    drop(crashed);
    settled("a crashed caller's redirect is undone", 0);
    assert_eq!(system.hosts(), before);

    let _held = system.leave_a_redirect(&bench.installed.cartridge);
    let problems = crate::cartridge::install::uninstall(
        &mut bench.store.lock_or_recover(),
        &bench.installed.cartridge,
        platform(),
        &bench.scratch.path("logs"),
        bench.helper.as_deref(),
        None,
    )
    .unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(system.hosts(), before);
}

const NEEDS_HOSTS: &str = "[lifecycle.service]\nprogram = \"main\"\n\n[capabilities.hosts]\nnames = [\"cabinet-test.invalid\"]\ntarget = \"service\"\nscope = \"session\"\n";

#[test]
fn withheld_capabilities_block_hosting_where_a_helper_exists() {
    use crate::cartridge::privileged::tests::FakeHelper;
    let mut bench = bench("rt-withheld", NEEDS_HOSTS);
    assert_eq!(bench.prepared().grant, Grant::Withheld);
    let helper = Arc::new(FakeHelper::default());
    let applied = Arc::clone(&helper.applied);
    bench.helper = Some(helper);
    let err = Session::host(
        bench.prepared(),
        me("nHOST"),
        HostOptions {
            bind: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            port: 0,
            address: "127.0.0.1".to_string(),
            app_version: "0.1.15".to_string(),
            verify: Arc::new(|_, _| true),
            log_dir: bench.scratch.path("logs"),
            helper: bench.helper.clone(),
        },
        quiet(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(err.contains("not approved"), "{err}");
    assert!(applied.lock_or_recover().is_empty());

    bench.helper = None;
    let host = bench.host("nHOST");
    block_on(host.stop());
}

fn kind(widget: &Widget) -> &'static str {
    match widget {
        Widget::Seats { .. } => "seats",
        Widget::Score { .. } => "score",
        Widget::Queue { .. } => "queue",
        Widget::Prompt { .. } => "prompt",
        Widget::Text { .. } => "text",
        Widget::Button { .. } => "button",
    }
}

#[test]
fn the_test_cartridge_runs_end_to_end_on_loopback() {
    let scratch_dir = Scratch::new("rt-echo-scratch");
    let touch = scratch_dir.path("touch-test.txt");
    #[cfg_attr(not(unix), allow(unused_mut))]
    let (mut bench, publisher) = test_cartridge(
        "rt-echo",
        Map::from_iter([(
            "scratch_dir".to_string(),
            json!(scratch_dir.path("").to_string_lossy()),
        )]),
    );
    assert_eq!(
        bench.installed.cartridge,
        CartridgeRef::new(Some(&publisher), "test")
    );
    assert_eq!(bench.installed.capabilities, Grant::Granted);
    {
        let store = bench.store.lock().unwrap();
        let view = store.registry().publisher_view(Some(&publisher));
        assert_eq!(view.standing, Standing::Trusted);
        assert_eq!(view.name.as_deref(), Some("Test publisher"));
        let entry = store.entry(&bench.installed.cartridge).unwrap();
        assert_eq!(store.grant(entry), Grant::Granted);
    }
    assert_eq!(
        std::fs::read_to_string(&touch).unwrap(),
        echo_cart::TOUCH_TEXT
    );

    #[cfg(unix)]
    let system = FakeSystem::start(&bench.scratch);
    #[cfg(unix)]
    {
        bench.helper = Some(system.helper());
    }
    let host = bench.host("nHOST");
    let member = bench.join(&host, "nMEMBER", "friend").unwrap();
    #[cfg(unix)]
    assert_eq!(
        system.redirects(),
        0,
        "redirected before the session started"
    );
    block_on(host.start()).unwrap();
    let view = eventually("both sides to see each other", &host, |view| {
        view.room.members.iter().all(|m| m.status == Status::Ready)
            && public(view, "nHOST")["seen"]["nMEMBER"] == json!("nMEMBER")
            && public(view, "nMEMBER")["seen"]["nHOST"] == json!("nHOST")
    });
    assert_eq!(view.running, vec!["service", "session"]);
    assert_eq!(view.room.service, Some(ServiceState { ready: true }));
    assert_eq!(public(&view, "nHOST")["service"], json!("127.0.0.1"));
    assert_eq!(public(&view, "nMEMBER")["service"], json!("127.0.0.1"));
    assert_eq!(public(&view, "nMEMBER")["role"], json!("member"));
    let kinds: Vec<&str> = view.ui.iter().map(kind).collect();
    for expected in ["seats", "score", "queue", "prompt", "text", "button"] {
        assert!(kinds.contains(&expected), "no {expected} in {kinds:?}");
    }
    let seated = eventually("the member in a seat", &member, |view| {
        view.ui.iter().any(|widget| {
            matches!(widget, Widget::Seats { slots }
                if slots.iter().filter(|slot| slot.occupant.is_some()).count() == 2)
        })
    });
    assert_eq!(seated.running, vec!["session"]);

    member.action("bump");
    eventually("the member's action", &host, |view| {
        public(view, "nMEMBER")["actions"] == json!(["bump"])
    });
    let pids = [
        public(&view, "nHOST")["pid"].clone(),
        public(&view, "nMEMBER")["pid"].clone(),
    ];
    #[cfg(unix)]
    assert_eq!(system.redirects(), 2, "{}", system.hosts());
    #[cfg(target_os = "macos")]
    {
        let allowed = system.firewall.0.lock_or_recover().clone();
        assert_eq!(allowed.len(), 1, "{allowed:?}");
        assert!(allowed[0].ends_with("/echo-cart"), "{allowed:?}");
    }
    #[cfg(target_os = "linux")]
    {
        let open = system.ports.0.lock_or_recover().clone();
        assert_eq!(
            open,
            vec![cabinet_helper::protocol::Port {
                proto: cabinet_helper::protocol::Proto::Tcp,
                port: 987
            }]
        );
        let granted = system.low_ports.0.lock_or_recover().clone();
        assert_eq!(granted.len(), 1, "{granted:?}");
        assert!(granted[0].ends_with("/echo-cart"), "{granted:?}");
    }
    #[cfg(target_os = "macos")]
    assert_eq!(
        echo_cart::bind_only(987).err().map(|err| err.kind()),
        Some(std::io::ErrorKind::AddrInUse),
        "the service does not hold its low port"
    );
    block_on(member.stop());
    #[cfg(unix)]
    assert_eq!(system.redirects(), 1, "{}", system.hosts());
    block_on(host.stop());
    #[cfg(unix)]
    assert_eq!(system.hosts(), FakeSystem::HOSTS);
    assert!(host.view().running.is_empty());
    assert!(member.view().running.is_empty());
    #[cfg(target_os = "macos")]
    echo_cart::bind_only(987).expect("the low port is free again after the session");
    #[cfg(unix)]
    assert!(system.firewall.0.lock_or_recover().is_empty());
    #[cfg(unix)]
    assert!(system.low_ports.0.lock_or_recover().is_empty());
    #[cfg(unix)]
    assert!(system.ports.0.lock_or_recover().is_empty());
    #[cfg(unix)]
    let _left_behind = system.leave_a_redirect(&bench.installed.cartridge);
    #[cfg(unix)]
    for pid in &pids {
        assert!(gone(pid), "pid {pid} still running");
    }
    let _ = pids;

    let problems = crate::cartridge::install::uninstall(
        &mut bench.store.lock_or_recover(),
        &bench.installed.cartridge,
        platform(),
        &bench.scratch.path("logs"),
        bench.helper.as_deref(),
        None,
    )
    .unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    assert!(!touch.exists(), "the touch file outlived the uninstall");
    #[cfg(unix)]
    assert_eq!(system.hosts(), FakeSystem::HOSTS);
}
