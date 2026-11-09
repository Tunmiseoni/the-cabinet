use cabinet_cartridge::protocol::{
    decode, encode, FromProgram, Line, SessionStart, ToProgram, Widget,
};
use serde_json::{json, Value};
use std::io::BufRead;

pub(crate) const UNIX: &str = "#!/bin/sh\nCABINET_FAKE_PROGRAM=1 CABINET_FAKE_ARGS=\"$*\" exec \"$CABINET_TEST_EXE\" --exact cartridge::fake::fake_program --nocapture --test-threads=1 -q\n";

pub(crate) const WINDOWS: &str = "@echo off\r\nset CABINET_FAKE_PROGRAM=1\r\nset \"CABINET_FAKE_ARGS=%*\"\r\n\"%CABINET_TEST_EXE%\" --exact cartridge::fake::fake_program --nocapture --test-threads=1 -q\r\nexit /b %ERRORLEVEL%\r\n";

pub(crate) fn install_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("CABINET_TEST_EXE", std::env::current_exe().unwrap());
    });
}

#[test]
fn fake_program() {
    if std::env::var("CABINET_FAKE_PROGRAM").as_deref() != Ok("1") {
        return;
    }
    let args: Vec<String> = std::env::var("CABINET_FAKE_ARGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    std::process::exit(run(&args));
}

fn say(message: &FromProgram) {
    println!("{}", encode(message));
}

fn publish(public: &Value, name: &str) {
    say(&FromProgram::State {
        public: public.clone(),
        ui: vec![Widget::Text {
            value: format!("hello {name}"),
        }],
    });
}

fn run(args: &[String]) -> i32 {
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines().map_while(Result::ok);
    let start: SessionStart = loop {
        let Some(line) = lines.next() else {
            return 10;
        };
        match decode::<ToProgram>(&line) {
            Line::Message(ToProgram::SessionStart(start)) => break start,
            _ => return 11,
        }
    };
    if has("--service") {
        say(&FromProgram::Ready {});
        for line in lines {
            if let Line::Message(ToProgram::Stop {}) = decode::<ToProgram>(&line) {
                return 0;
            }
        }
        return 0;
    }
    if has("--fail") {
        say(&FromProgram::Error {
            message: "fake failure".to_string(),
            fatal: true,
        });
        return 3;
    }
    let name = start.me.name.clone();
    let mut public = json!({
        "hello": start.me.id,
        "pid": std::process::id(),
        "role": start.role,
        "service": start.service.as_ref().map(|service| service.address.clone()),
        "seen": {},
        "members": start.members.len(),
        "actions": [],
    });
    let mut stubborn = has("--ignore-stop");
    say(&FromProgram::Ready {});
    publish(&public, &name);
    for line in lines {
        let Line::Message(message) = decode::<ToProgram>(&line) else {
            continue;
        };
        match message {
            ToProgram::MemberState { member_id, state } => {
                public["seen"][member_id] = state["hello"].clone();
                publish(&public, &name);
            }
            ToProgram::RoomChanged { members } => {
                public["members"] = json!(members.len());
                publish(&public, &name);
            }
            ToProgram::Action { id } => {
                public["actions"]
                    .as_array_mut()
                    .expect("actions is an array")
                    .push(json!(id));
                publish(&public, &name);
                match id.as_str() {
                    "exit" => return 0,
                    "crash" => return 4,
                    "stubborn" => stubborn = true,
                    _ => {}
                }
            }
            ToProgram::JoinRequest { member } => say(&FromProgram::JoinAnswer {
                allow: !member.name.contains("deny"),
                member_id: member.id,
                reason: Some("no deny".to_string()),
            }),
            ToProgram::Stop {} if !stubborn => return 0,
            _ => {}
        }
    }
    0
}
