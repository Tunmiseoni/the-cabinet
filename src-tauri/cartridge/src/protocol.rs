use crate::platform::Platform;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Member,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths {
    pub cartridge: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStart {
    pub role: Role,
    #[serde(rename = "self")]
    pub me: Me,
    pub members: Vec<Member>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<Service>,
    #[serde(default)]
    pub settings: Map<String, Value>,
    pub paths: Paths,
    pub platform: Platform,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToProgram {
    #[serde(rename = "session.start")]
    SessionStart(SessionStart),
    #[serde(rename = "room.changed")]
    RoomChanged { members: Vec<Member> },
    #[serde(rename = "member.state")]
    MemberState { member_id: String, state: Value },
    #[serde(rename = "join.request")]
    JoinRequest { member: Member },
    #[serde(rename = "action")]
    Action { id: String },
    #[serde(rename = "stop")]
    Stop {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Widget {
    Seats {
        slots: Vec<Slot>,
    },
    Score {
        left: i64,
        right: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Queue {
        names: Vec<String>,
    },
    Prompt {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    Text {
        value: String,
    },
    Button {
        id: String,
        label: String,
    },
}

fn known_widgets<'de, D>(deserializer: D) -> Result<Vec<Widget>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FromProgram {
    #[serde(rename = "ready")]
    Ready {},
    #[serde(rename = "state")]
    State {
        #[serde(default)]
        public: Value,
        #[serde(default, deserialize_with = "known_widgets")]
        ui: Vec<Widget>,
    },
    #[serde(rename = "join.answer")]
    JoinAnswer {
        member_id: String,
        allow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default)]
        fatal: bool,
    },
    #[serde(rename = "log")]
    Log { level: Level, message: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line<T> {
    Message(T),
    Text(String),
    Invalid { line: String, error: String },
}

pub fn encode<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).expect("protocol messages always serialize")
}

pub fn decode<T: DeserializeOwned>(line: &str) -> Line<T> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return Line::Text(line.to_string());
    }
    match serde_json::from_str(trimmed) {
        Ok(message) => Line::Message(message),
        Err(err) => Line::Invalid {
            line: line.to_string(),
            error: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start() -> ToProgram {
        ToProgram::SessionStart(SessionStart {
            role: Role::Host,
            me: Me {
                id: "nSELF".to_string(),
                name: "player-one".to_string(),
            },
            members: vec![Member {
                id: "nSELF".to_string(),
                name: "player-one".to_string(),
                address: "100.64.0.1".to_string(),
            }],
            service: Some(Service {
                address: "127.0.0.1".to_string(),
            }),
            settings: Map::from_iter([("scratch_dir".to_string(), json!("/tmp/x"))]),
            paths: Paths {
                cartridge: "/store/abc".to_string(),
                data: "/data/test".to_string(),
            },
            platform: Platform::LinuxX86_64,
        })
    }

    #[test]
    fn encodes_the_wire_shape() {
        let encoded: Value = serde_json::from_str(&encode(&start())).unwrap();
        assert_eq!(
            encoded,
            json!({
                "type": "session.start",
                "role": "host",
                "self": {"id": "nSELF", "name": "player-one"},
                "members": [{"id": "nSELF", "name": "player-one", "address": "100.64.0.1"}],
                "service": {"address": "127.0.0.1"},
                "settings": {"scratch_dir": "/tmp/x"},
                "paths": {"cartridge": "/store/abc", "data": "/data/test"},
                "platform": "linux-x86_64"
            })
        );
        assert_eq!(encode(&ToProgram::Stop {}), r#"{"type":"stop"}"#);
        assert_eq!(
            encode(&ToProgram::MemberState {
                member_id: "n2".to_string(),
                state: json!({"seat": 1})
            }),
            r#"{"type":"member.state","member_id":"n2","state":{"seat":1}}"#
        );
        assert!(!encode(&start()).contains('\n'));
    }

    #[test]
    fn round_trips_every_message() {
        let to_program = [
            start(),
            ToProgram::RoomChanged { members: vec![] },
            ToProgram::JoinRequest {
                member: Member {
                    id: "n2".to_string(),
                    name: "friend".to_string(),
                    address: "100.64.0.2".to_string(),
                },
            },
            ToProgram::Action {
                id: "rematch".to_string(),
            },
            ToProgram::Stop {},
        ];
        for message in to_program {
            assert_eq!(
                decode::<ToProgram>(&encode(&message)),
                Line::Message(message)
            );
        }
        let from_program = [
            FromProgram::Ready {},
            FromProgram::State {
                public: json!({"rotation": null}),
                ui: vec![
                    Widget::Seats {
                        slots: vec![Slot {
                            label: "P1".to_string(),
                            occupant: Some("player-one".to_string()),
                        }],
                    },
                    Widget::Score {
                        left: 2,
                        right: 1,
                        label: Some("FT3".to_string()),
                    },
                    Widget::Queue {
                        names: vec!["friend".to_string()],
                    },
                    Widget::Prompt {
                        text: "Insert coin".to_string(),
                        target: Some("n2".to_string()),
                    },
                    Widget::Text {
                        value: "ready".to_string(),
                    },
                    Widget::Button {
                        id: "rematch".to_string(),
                        label: "Rematch".to_string(),
                    },
                ],
            },
            FromProgram::JoinAnswer {
                member_id: "n2".to_string(),
                allow: false,
                reason: Some("full".to_string()),
            },
            FromProgram::Error {
                message: "boom".to_string(),
                fatal: true,
            },
            FromProgram::Log {
                level: Level::Warn,
                message: "hmm".to_string(),
            },
        ];
        for message in from_program {
            assert_eq!(
                decode::<FromProgram>(&encode(&message)),
                Line::Message(message)
            );
        }
    }

    #[test]
    fn decodes_minimal_program_lines() {
        assert_eq!(
            decode::<FromProgram>(r#"{"type":"ready"}"#),
            Line::Message(FromProgram::Ready {})
        );
        assert_eq!(
            decode::<FromProgram>(r#"{"type":"error","message":"x"}"#),
            Line::Message(FromProgram::Error {
                message: "x".to_string(),
                fatal: false
            })
        );
        assert_eq!(
            decode::<FromProgram>(r#"  {"type":"state"}  "#),
            Line::Message(FromProgram::State {
                public: Value::Null,
                ui: vec![]
            })
        );
    }

    #[test]
    fn drops_widgets_this_cabinet_does_not_know() {
        let line = r#"{"type":"state","public":{},"ui":[{"kind":"hologram"},{"kind":"text","value":"hi"},{"kind":"score"}]}"#;
        assert_eq!(
            decode::<FromProgram>(line),
            Line::Message(FromProgram::State {
                public: json!({}),
                ui: vec![Widget::Text {
                    value: "hi".to_string()
                }],
            })
        );
    }

    #[test]
    fn separates_text_from_broken_json() {
        assert_eq!(
            decode::<FromProgram>("starting up"),
            Line::Text("starting up".to_string())
        );
        assert!(matches!(
            decode::<FromProgram>(r#"{"type":"teleport"}"#),
            Line::Invalid { .. }
        ));
        assert!(matches!(
            decode::<FromProgram>("{not json"),
            Line::Invalid { .. }
        ));
    }
}
