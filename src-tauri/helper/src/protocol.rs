use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

pub const VERSION: u32 = 1;
pub const LABEL: &str = "com.the-cabinet.helper";
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Session,
    Install,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    Hosts {
        names: Vec<String>,
        target: Ipv4Addr,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub owner: String,
    pub scope: Scope,
    pub change: Change,
    pub applied_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {},
    Apply {
        owner: String,
        scope: Scope,
        change: Change,
    },
    Release {},
    Forget {
        owner: String,
    },
    Sweep {},
    Status {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello { version: u32 },
    Applied { id: String },
    Done { undone: usize },
    Status { entries: Vec<Entry> },
    Error { message: String },
}

pub fn encode<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).expect("protocol messages serialize")
}
