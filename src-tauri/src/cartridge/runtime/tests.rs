use super::*;
use crate::cartridge::assets::tests::FakeFetch;
use crate::cartridge::install::Installer;
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
    }
    .install(&cart, None, &Approval::default())
    .unwrap();
    Bench {
        scratch,
        store: Arc::new(Mutex::new(store)),
        installed,
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
    let binary = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("echo-cart{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "{} is missing: build it with `cargo build -p echo-cart` (`cargo test --workspace` does)",
        binary.display()
    );
    binary
}

fn test_cartridge(tag: &str, settings: Map<String, Value>) -> (Bench, Publisher) {
    let scratch = Scratch::new(tag);
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
    };
    (bench, signer.publisher())
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
    let (bench, publisher) = test_cartridge(
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

    let host = bench.host("nHOST");
    let member = bench.join(&host, "nMEMBER", "friend").unwrap();
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
    block_on(member.stop());
    block_on(host.stop());
    assert!(host.view().running.is_empty());
    assert!(member.view().running.is_empty());
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
    )
    .unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    assert!(!touch.exists(), "the touch file outlived the uninstall");
}
