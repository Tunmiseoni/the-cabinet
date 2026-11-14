use cabinet_cartridge::protocol::{
    decode, encode, FromProgram, Level, Line, Me, Member, SessionStart, Slot, ToProgram, Widget,
};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const TOUCH_TEXT: &str = "touched by echo-cart\n";
pub const SEATS: usize = 2;

pub fn run(args: &[String], input: impl BufRead, output: impl Write) -> i32 {
    let mut out = Out(output);
    match args.first().map(String::as_str) {
        Some("setup") => return setup(args.get(1)),
        Some("uninstall") => return 0,
        Some("bind") => {
            return match args
                .get(1)
                .and_then(|port| port.parse().ok())
                .map(bind_only)
            {
                Some(Ok(_)) => 0,
                _ => 14,
            }
        }
        _ => {}
    }
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let mut lines = input.lines().map_while(Result::ok);
    let start = match lines.next().map(|line| decode::<ToProgram>(&line)) {
        Some(Line::Message(ToProgram::SessionStart(start))) => start,
        None => return 10,
        Some(_) => return 11,
    };
    if has("--service") {
        let bind = args
            .iter()
            .position(|arg| arg == "--bind")
            .and_then(|at| args.get(at + 1));
        return service(bind, lines, &mut out);
    }
    if has("--fail") {
        out.say(&FromProgram::Error {
            message: "echo-cart was asked to fail".to_string(),
            fatal: true,
        });
        return 3;
    }
    Session::new(start).run(lines, &mut out)
}

fn setup(path: Option<&String>) -> i32 {
    let Some(path) = path else {
        return 12;
    };
    match std::fs::write(path, TOUCH_TEXT) {
        Ok(()) => 0,
        Err(_) => 13,
    }
}

struct Out<W>(W);

impl<W: Write> Out<W> {
    fn say(&mut self, message: &FromProgram) {
        let _ = writeln!(self.0, "{}", encode(message));
        let _ = self.0.flush();
    }

    fn log(&mut self, message: String) {
        self.say(&FromProgram::Log {
            level: Level::Info,
            message,
        });
    }
}

pub fn bind_only(port: u16) -> std::io::Result<socket2::Socket> {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
    socket.bind(&address.into())?;
    Ok(socket)
}

fn service(
    bind: Option<&String>,
    lines: impl Iterator<Item = String>,
    out: &mut Out<impl Write>,
) -> i32 {
    let _held = bind.and_then(|port| {
        let bound = port
            .parse::<u16>()
            .map_err(|err| err.to_string())
            .and_then(|port| bind_only(port).map_err(|err| err.to_string()));
        match bound {
            Ok(socket) => {
                out.log(format!("echo-cart bound port {port}"));
                Some(socket)
            }
            Err(err) => {
                out.log(format!("echo-cart cannot bind port {port}: {err}"));
                None
            }
        }
    });
    out.say(&FromProgram::Ready {});
    out.log("echo-cart service up".to_string());
    for line in lines {
        if let Line::Message(ToProgram::Stop {}) = decode::<ToProgram>(&line) {
            return 0;
        }
    }
    0
}

struct Session {
    me: Me,
    members: Vec<Member>,
    public: Value,
    stubborn: bool,
}

impl Session {
    fn new(start: SessionStart) -> Self {
        let public = json!({
            "hello": start.me.id,
            "pid": std::process::id(),
            "role": start.role,
            "service": start.service.as_ref().map(|service| service.address.clone()),
            "seen": {},
            "members": start.members.len(),
            "actions": [],
        });
        Self {
            me: start.me,
            members: start.members,
            public,
            stubborn: false,
        }
    }

    fn count(&self, key: &str) -> i64 {
        match &self.public[key] {
            Value::Array(items) => items.len() as i64,
            Value::Object(items) => items.len() as i64,
            _ => 0,
        }
    }

    fn ui(&self) -> Vec<Widget> {
        let names: Vec<String> = self.members.iter().map(|m| m.name.clone()).collect();
        let button = |id: &str, label: &str| Widget::Button {
            id: id.to_string(),
            label: label.to_string(),
        };
        vec![
            Widget::Text {
                value: format!("hello {}", self.me.name),
            },
            Widget::Seats {
                slots: (0..SEATS)
                    .map(|seat| Slot {
                        label: format!("Seat {}", seat + 1),
                        occupant: names.get(seat).cloned(),
                    })
                    .collect(),
            },
            Widget::Score {
                left: self.count("actions"),
                right: self.count("seen"),
                label: Some("actions · peers seen".to_string()),
            },
            Widget::Queue {
                names: names.iter().skip(SEATS).cloned().collect(),
            },
            Widget::Prompt {
                text: "Press Bump to send an action".to_string(),
                target: Some(self.me.id.clone()),
            },
            button("bump", "Bump"),
            button("exit", "Leave"),
            button("crash", "Crash"),
            button("stubborn", "Ignore stop"),
        ]
    }

    fn publish(&self, out: &mut Out<impl Write>) {
        out.say(&FromProgram::State {
            public: self.public.clone(),
            ui: self.ui(),
        });
    }

    fn run(mut self, lines: impl Iterator<Item = String>, out: &mut Out<impl Write>) -> i32 {
        out.say(&FromProgram::Ready {});
        out.log(format!("echo-cart session for {}", self.me.name));
        self.publish(out);
        for line in lines {
            let Line::Message(message) = decode::<ToProgram>(&line) else {
                continue;
            };
            match message {
                ToProgram::MemberState { member_id, state } => {
                    self.public["seen"][member_id] = state["hello"].clone();
                    self.publish(out);
                }
                ToProgram::RoomChanged { members } => {
                    self.public["members"] = json!(members.len());
                    self.members = members;
                    self.publish(out);
                }
                ToProgram::Action { id } => {
                    if let Some(actions) = self.public["actions"].as_array_mut() {
                        actions.push(json!(id));
                    }
                    self.publish(out);
                    match id.as_str() {
                        "exit" => return 0,
                        "crash" => return 4,
                        "stubborn" => self.stubborn = true,
                        _ => {}
                    }
                }
                ToProgram::JoinRequest { member } => out.say(&FromProgram::JoinAnswer {
                    allow: !member.name.contains("deny"),
                    member_id: member.id,
                    reason: Some("no deny".to_string()),
                }),
                ToProgram::Stop {} if !self.stubborn => return 0,
                _ => {}
            }
        }
        0
    }
}
