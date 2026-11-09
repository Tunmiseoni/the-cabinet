use crate::sync::MutexExt;
use cabinet_cartridge::manifest::LateJoin;
use cabinet_cartridge::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::net::IpAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response};

pub const PROTOCOL: u32 = 2;
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Gathering,
    Running,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Joined,
    Starting,
    Ready,
    Error,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomCartridge {
    pub publisher: String,
    pub id: String,
    pub version: Version,
    pub digest: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomMember {
    pub id: String,
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub public: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceState {
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub protocol: u32,
    pub room_id: String,
    pub host: String,
    pub cartridge: RoomCartridge,
    pub max_members: u32,
    pub late_join: LateJoin,
    pub phase: Phase,
    #[serde(default)]
    pub service: Option<ServiceState>,
    pub members: Vec<RoomMember>,
    #[serde(default)]
    pub message: Option<String>,
    pub revision: u64,
}

impl Room {
    pub fn member(&self, id: &str) -> Option<&RoomMember> {
        self.members.iter().find(|member| member.id == id)
    }

    pub fn member_mut(&mut self, id: &str) -> Option<&mut RoomMember> {
        self.members.iter_mut().find(|member| member.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub app: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Joiner {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub member: Joiner,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateUpdate {
    pub member_id: String,
    #[serde(default)]
    pub status: Option<Status>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub public: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leave {
    pub member_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
}

pub trait RoomHooks: Send + Sync {
    fn changed(&self, room: &Room);
    fn ask_join(&self, member: &RoomMember) -> Result<(), String>;
    fn verify(&self, member_id: &str, address: IpAddr) -> bool;
}

pub struct RoomHost {
    room: Mutex<Room>,
    wake: Condvar,
    hooks: Arc<dyn RoomHooks>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    Closed,
    Mismatch,
    Full,
    Late,
    Refused(String),
    Unverified,
}

impl JoinError {
    fn status(&self) -> u16 {
        match self {
            JoinError::Mismatch => 409,
            JoinError::Unverified => 403,
            _ => 423,
        }
    }

    pub fn message(&self) -> String {
        match self {
            JoinError::Closed => "the room is closed".to_string(),
            JoinError::Mismatch => {
                "your cartridge build differs from the room's; update it first".to_string()
            }
            JoinError::Full => "the room is full".to_string(),
            JoinError::Late => "this game does not take players once it has started".to_string(),
            JoinError::Refused(reason) => reason.clone(),
            JoinError::Unverified => {
                "your address does not match your network identity".to_string()
            }
        }
    }
}

impl RoomHost {
    pub fn new(room: Room, hooks: Arc<dyn RoomHooks>) -> Self {
        Self {
            room: Mutex::new(room),
            wake: Condvar::new(),
            hooks,
        }
    }

    pub fn snapshot(&self) -> Room {
        self.room.lock_or_recover().clone()
    }

    pub fn update<R>(&self, change: impl FnOnce(&mut Room) -> R) -> R {
        let (result, room) = {
            let mut room = self.room.lock_or_recover();
            let before = room.clone();
            let result = change(&mut room);
            if *room != before {
                room.revision = room.revision.wrapping_add(1);
            }
            (result, (*room != before).then(|| room.clone()))
        };
        if let Some(room) = room {
            self.wake.notify_all();
            self.hooks.changed(&room);
        }
        result
    }

    pub fn wait_newer(&self, since: u64, timeout: Duration) -> Room {
        let deadline = Instant::now() + timeout;
        let mut room = self.room.lock_or_recover();
        loop {
            if room.revision != since || room.phase == Phase::Closed {
                return room.clone();
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return room.clone();
            };
            room = self
                .wake
                .wait_timeout(room, left)
                .unwrap_or_else(|err| err.into_inner())
                .0;
        }
    }

    pub fn join(&self, request: &JoinRequest, address: IpAddr) -> Result<Room, JoinError> {
        let member = RoomMember {
            id: request.member.id.clone(),
            name: request.member.name.clone(),
            address: address.to_string(),
            status: Status::Joined,
            message: None,
            public: Value::Null,
        };
        if !address.is_loopback() && !self.hooks.verify(&member.id, address) {
            return Err(JoinError::Unverified);
        }
        let ask = {
            let room = self.room.lock_or_recover();
            if room.phase == Phase::Closed {
                return Err(JoinError::Closed);
            }
            if room.cartridge.digest != request.digest {
                return Err(JoinError::Mismatch);
            }
            if room.member(&member.id).is_some() {
                None
            } else {
                if room.members.len() as u32 >= room.max_members {
                    return Err(JoinError::Full);
                }
                match (room.phase, room.late_join) {
                    (Phase::Running, LateJoin::Never) => return Err(JoinError::Late),
                    (Phase::Running, LateJoin::Ask) => Some(()),
                    _ => None,
                }
            }
        };
        if ask.is_some() {
            self.hooks.ask_join(&member).map_err(JoinError::Refused)?;
        }
        self.update(|room| {
            if room.phase == Phase::Closed {
                return Err(JoinError::Closed);
            }
            match room.member_mut(&member.id) {
                Some(existing) => {
                    existing.address = member.address.clone();
                    existing.name = member.name.clone();
                }
                None => {
                    if room.members.len() as u32 >= room.max_members {
                        return Err(JoinError::Full);
                    }
                    room.members.push(member);
                }
            }
            Ok(())
        })?;
        Ok(self.snapshot())
    }

    fn owns(&self, member_id: &str, address: IpAddr) -> bool {
        self.room
            .lock_or_recover()
            .member(member_id)
            .is_some_and(|member| member.address == address.to_string())
    }

    pub fn apply(&self, update: &StateUpdate) -> bool {
        self.update(|room| match room.member_mut(&update.member_id) {
            Some(member) => {
                if let Some(status) = update.status {
                    member.status = status;
                }
                if update.message.is_some() || update.status.is_some() {
                    member.message = update.message.clone();
                }
                if let Some(public) = &update.public {
                    member.public = public.clone();
                }
                true
            }
            None => false,
        })
    }

    pub fn leave(&self, member_id: &str) {
        self.update(|room| {
            if member_id != room.host {
                room.members.retain(|member| member.id != member_id);
            }
        });
    }

    pub fn close(&self, message: Option<String>) {
        self.update(|room| {
            room.phase = Phase::Closed;
            if message.is_some() {
                room.message = message;
            }
        });
    }

    pub fn handle(&self, mut request: Request, app_version: &str) {
        let address = request.remote_addr().map(|socket| socket.ip());
        let url = request.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        let method = request.method().clone();
        let response = match (method, path) {
            (Method::Get, "/v2/hello") => json(
                200,
                &Hello {
                    protocol: PROTOCOL,
                    app: app_version.to_string(),
                },
            ),
            (Method::Get, "/v2/room") => {
                let since = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("since="))
                    .and_then(|value| value.parse().ok());
                let room = match since {
                    Some(since) => self.wait_newer(since, crate::constants::ROOM_LONG_POLL),
                    None => self.snapshot(),
                };
                json(200, &room)
            }
            (Method::Post, path) => match (address, read_body(&mut request)) {
                (None, _) => refuse(400, "unknown sender"),
                (_, Err(err)) => refuse(400, &err),
                (Some(address), Ok(body)) => self.post(path, &body, address),
            },
            (Method::Get, "/room") => refuse(410, "this host runs The Cabinet 2; update to join"),
            _ => refuse(404, "not found"),
        };
        let _ = request.respond(response);
    }

    fn post(&self, path: &str, body: &str, address: IpAddr) -> Response<std::io::Cursor<Vec<u8>>> {
        match path {
            "/v2/join" => match serde_json::from_str::<JoinRequest>(body) {
                Ok(join) => match self.join(&join, address) {
                    Ok(room) => json(200, &room),
                    Err(err) => refuse(err.status(), &err.message()),
                },
                Err(err) => refuse(400, &err.to_string()),
            },
            "/v2/state" => match serde_json::from_str::<StateUpdate>(body) {
                Ok(update) if self.owns(&update.member_id, address) => {
                    self.apply(&update);
                    json(200, &self.snapshot())
                }
                Ok(_) => refuse(403, "not a member of this room"),
                Err(err) => refuse(400, &err.to_string()),
            },
            "/v2/leave" => match serde_json::from_str::<Leave>(body) {
                Ok(leave) if self.owns(&leave.member_id, address) => {
                    self.leave(&leave.member_id);
                    json(200, &self.snapshot())
                }
                Ok(_) => refuse(403, "not a member of this room"),
                Err(err) => refuse(400, &err.to_string()),
            },
            _ => refuse(404, "not found"),
        }
    }
}

fn read_body(request: &mut Request) -> Result<String, String> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|err| format!("unreadable body: {err}"))?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err("body too large".to_string());
    }
    Ok(body)
}

fn json<T: Serialize>(status: u16, value: &T) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header");
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header)
}

fn refuse(status: u16, error: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    json(
        status,
        &Refusal {
            error: error.to_string(),
        },
    )
}

#[derive(Debug, Clone)]
pub struct RoomClient {
    base: String,
    agent: ureq::Agent,
}

impl RoomClient {
    pub fn new(host: &str, port: u16) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(
                crate::constants::ROOM_LONG_POLL + crate::constants::ROOM_REQUEST_TIMEOUT,
            ))
            .http_status_as_error(false)
            .build()
            .into();
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        Self {
            base: format!("http://{host}:{port}"),
            agent,
        }
    }

    fn read<T: serde::de::DeserializeOwned>(
        mut response: ureq::http::Response<ureq::Body>,
    ) -> Result<T, String> {
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|err| format!("unreadable reply: {err}"))?;
        if status == 200 {
            serde_json::from_str(&text).map_err(|err| format!("unexpected reply: {err}"))
        } else {
            Err(serde_json::from_str::<Refusal>(&text)
                .map(|refusal| refusal.error)
                .unwrap_or_else(|_| format!("the host answered {status}")))
        }
    }

    pub fn hello(&self) -> Result<Hello, String> {
        let response = self
            .agent
            .get(format!("{}/v2/hello", self.base))
            .call()
            .map_err(|err| format!("cannot reach the host: {err}"))?;
        if response.status().as_u16() == 404 {
            return Err(
                "the host runs an older Cabinet; both of you need The Cabinet 2".to_string(),
            );
        }
        let hello: Hello = Self::read(response)?;
        if hello.protocol != PROTOCOL {
            return Err(format!(
                "the host runs room protocol {} (The Cabinet {}); this Cabinet speaks {PROTOCOL}",
                hello.protocol, hello.app
            ));
        }
        Ok(hello)
    }

    fn post<B: Serialize>(&self, path: &str, body: &B) -> Result<Room, String> {
        let body = serde_json::to_string(body).map_err(|err| err.to_string())?;
        let response = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|err| format!("cannot reach the host: {err}"))?;
        Self::read(response)
    }

    pub fn join(&self, request: &JoinRequest) -> Result<Room, String> {
        self.post("/v2/join", request)
    }

    pub fn update(&self, update: &StateUpdate) -> Result<Room, String> {
        self.post("/v2/state", update)
    }

    pub fn leave(&self, member_id: &str) -> Result<Room, String> {
        self.post(
            "/v2/leave",
            &Leave {
                member_id: member_id.to_string(),
            },
        )
    }

    pub fn room(&self, since: Option<u64>) -> Result<Room, String> {
        let url = match since {
            Some(since) => format!("{}/v2/room?since={since}", self.base),
            None => format!("{}/v2/room", self.base),
        };
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| format!("cannot reach the host: {err}"))?;
        Self::read(response)
    }
}

#[cfg(test)]
mod tests;
