use cabinet_cartridge::protocol::{
    decode, encode, FromProgram, Line, Me, Member, Paths, Role, Service, SessionStart, ToProgram,
    Widget,
};
use cabinet_cartridge::Platform;
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Program {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Program {
    fn start(args: &[&str], role: Role) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_echo-cart"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut program = Self {
            child,
            stdin,
            stdout,
        };
        program.send(&ToProgram::SessionStart(start(role)));
        program
    }

    fn send(&mut self, message: &ToProgram) {
        writeln!(self.stdin, "{}", encode(message)).unwrap();
        self.stdin.flush().unwrap();
    }

    fn next(&mut self) -> FromProgram {
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "eof");
            if let Line::Message(message) = decode::<FromProgram>(&line) {
                if !matches!(message, FromProgram::Log { .. }) {
                    return message;
                }
            }
        }
    }

    fn state(&mut self) -> (Value, Vec<Widget>) {
        match self.next() {
            FromProgram::State { public, ui } => (public, ui),
            other => panic!("expected state, got {other:?}"),
        }
    }

    fn exit_code(mut self) -> Option<i32> {
        drop(self.stdin);
        self.child.wait().unwrap().code()
    }
}

fn member(id: &str, name: &str) -> Member {
    Member {
        id: id.to_string(),
        name: name.to_string(),
        address: "127.0.0.1".to_string(),
    }
}

fn start(role: Role) -> SessionStart {
    SessionStart {
        role,
        me: Me {
            id: "nSELF".to_string(),
            name: "player-one".to_string(),
        },
        members: vec![member("nSELF", "player-one")],
        service: Some(Service {
            address: "127.0.0.1".to_string(),
        }),
        settings: Map::new(),
        paths: Paths {
            cartridge: "/store/abc".to_string(),
            data: "/data/test".to_string(),
        },
        platform: Platform::current().unwrap(),
    }
}

#[test]
fn a_session_publishes_every_widget_and_reacts_to_events() {
    let mut program = Program::start(&[], Role::Host);
    assert_eq!(program.next(), FromProgram::Ready {});
    let (public, ui) = program.state();
    assert_eq!(public["hello"], json!("nSELF"));
    assert_eq!(public["role"], json!("host"));
    assert_eq!(public["service"], json!("127.0.0.1"));
    let kinds: Vec<&str> = ui
        .iter()
        .map(|widget| match widget {
            Widget::Seats { .. } => "seats",
            Widget::Score { .. } => "score",
            Widget::Queue { .. } => "queue",
            Widget::Prompt { .. } => "prompt",
            Widget::Text { .. } => "text",
            Widget::Button { .. } => "button",
        })
        .collect();
    for kind in ["seats", "score", "queue", "prompt", "text", "button"] {
        assert!(kinds.contains(&kind), "no {kind} widget in {kinds:?}");
    }

    program.send(&ToProgram::RoomChanged {
        members: vec![
            member("nSELF", "player-one"),
            member("n2", "friend"),
            member("n3", "third"),
        ],
    });
    let (public, ui) = program.state();
    assert_eq!(public["members"], json!(3));
    assert!(ui.contains(&Widget::Queue {
        names: vec!["third".to_string()]
    }));

    program.send(&ToProgram::MemberState {
        member_id: "n2".to_string(),
        state: json!({"hello": "n2"}),
    });
    assert_eq!(program.state().0["seen"], json!({"n2": "n2"}));

    program.send(&ToProgram::Action {
        id: "bump".to_string(),
    });
    let (public, ui) = program.state();
    assert_eq!(public["actions"], json!(["bump"]));
    assert!(ui.contains(&Widget::Score {
        left: 1,
        right: 1,
        label: Some("actions · peers seen".to_string())
    }));

    for (name, allow) in [("friend", true), ("deny me", false)] {
        program.send(&ToProgram::JoinRequest {
            member: member("n4", name),
        });
        match program.next() {
            FromProgram::JoinAnswer {
                member_id,
                allow: answer,
                ..
            } => {
                assert_eq!(member_id, "n4");
                assert_eq!(answer, allow, "{name}");
            }
            other => panic!("expected join.answer, got {other:?}"),
        }
    }

    program.send(&ToProgram::Stop {});
    assert_eq!(program.exit_code(), Some(0));
}

#[test]
fn the_crash_button_exits_with_a_failure() {
    let mut program = Program::start(&[], Role::Member);
    program.next();
    program.state();
    program.send(&ToProgram::Action {
        id: "crash".to_string(),
    });
    program.state();
    assert_eq!(program.exit_code(), Some(4));
}

#[test]
fn the_service_is_ready_and_stops_on_request() {
    let mut program = Program::start(&["--service"], Role::Host);
    assert_eq!(program.next(), FromProgram::Ready {});
    program.send(&ToProgram::Stop {});
    assert_eq!(program.exit_code(), Some(0));
}

#[test]
fn setup_writes_the_declared_touch() {
    let dir = std::env::temp_dir().join(format!("echo-cart-setup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let touch = dir.join("touch-test.txt");
    let status = Command::new(env!("CARGO_BIN_EXE_echo-cart"))
        .arg("setup")
        .arg(&touch)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(&touch).unwrap(),
        echo_cart::TOUCH_TEXT
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
