use super::assets::{self, CheckFailure};
use super::context::Context;
use super::privileged::{Privileges, SharedHelper};
use super::registry::{self, CartridgeRef};
use super::room::{
    JoinRequest, Joiner, Phase, Room, RoomCartridge, RoomClient, RoomHooks, RoomHost, RoomMember,
    ServiceState, StateUpdate, Status, PROTOCOL,
};
use super::store::Store;
use super::trust::Grant;
use crate::lobby::beacon::Beacon;
use crate::supervisor::{self, Capture, Exit, Stdin, Supervised};
use crate::sync::MutexExt;
use crate::tasks::{self, TaskTree};
use cabinet_cartridge::manifest::{Manifest, StepKind};
use cabinet_cartridge::protocol::{
    self, FromProgram, Level, Line, Member, Paths, Role, Service, SessionStart, ToProgram, Widget,
};
use cabinet_cartridge::Platform;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};

pub const LOG_TARGET: &str = "cartridge";

#[derive(Debug, Clone)]
pub struct Prepared {
    pub cartridge: CartridgeRef,
    pub digest: String,
    pub manifest: Manifest,
    pub cartridge_dir: PathBuf,
    pub data_dir: PathBuf,
    pub settings: Map<String, Value>,
    pub platform: Platform,
    pub grant: Grant,
}

impl Prepared {
    pub fn from_store(
        store: &Store,
        cartridge: &CartridgeRef,
        digest: Option<&str>,
        platform: Platform,
    ) -> crate::error::Result<Self> {
        let entry = store.entry(cartridge)?;
        let digest = match digest {
            Some(digest) => digest.to_string(),
            None => store.active_digest(cartridge)?,
        };
        if entry.held(&digest).is_none() {
            return Err(format!(
                "this room needs a build of {cartridge} you do not have ({}); install it first",
                &digest[..12.min(digest.len())]
            )
            .into());
        }
        let manifest = store.manifest(&digest)?;
        Ok(Self {
            cartridge: cartridge.clone(),
            grant: store.registry().grant(entry, &manifest.capabilities),
            manifest,
            cartridge_dir: store.unpacked(&digest),
            data_dir: store.data_dir(cartridge),
            settings: entry.settings.clone(),
            platform,
            digest,
        })
    }

    pub fn context(&self) -> Context<'_> {
        Context {
            manifest: &self.manifest,
            cartridge_dir: self.cartridge_dir.clone(),
            data_dir: self.data_dir.clone(),
            platform: self.platform,
            settings: &self.settings,
        }
    }

    pub fn checks(&self) -> Vec<CheckFailure> {
        assets::run_checks(&self.context())
    }

    pub fn preflight(&self) -> crate::error::Result<()> {
        let missing = registry::missing_required(&self.manifest, &self.settings);
        if !missing.is_empty() {
            return Err(format!(
                "{} needs these settings first: {}",
                self.manifest.title,
                missing.join(", ")
            )
            .into());
        }
        let failures = self.checks();
        if failures.is_empty() {
            Ok(())
        } else {
            let lines: Vec<String> = failures
                .iter()
                .map(|failure| format!("{}: {}", failure.label, failure.message))
                .collect();
            Err(lines.join("\n").into())
        }
    }

    fn privileges(&self, helper: Option<SharedHelper>) -> Privileges {
        Privileges::new(
            helper,
            &self.cartridge,
            &self.manifest.title,
            &self.manifest.capabilities,
            self.grant,
        )
    }

    fn room_cartridge(&self) -> RoomCartridge {
        RoomCartridge {
            publisher: self.cartridge.publisher.clone(),
            id: self.manifest.id.clone(),
            version: self.manifest.version.clone(),
            digest: self.digest.clone(),
            title: self.manifest.title.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Me {
    pub id: String,
    pub name: String,
}

pub type Verify = Arc<dyn Fn(&str, IpAddr) -> bool + Send + Sync>;

pub struct HostOptions {
    pub bind: Vec<IpAddr>,
    pub port: u16,
    pub address: String,
    pub app_version: String,
    pub verify: Verify,
    pub log_dir: PathBuf,
    pub helper: Option<SharedHelper>,
}

pub struct JoinOptions {
    pub host: String,
    pub port: u16,
    pub log_dir: PathBuf,
    pub helper: Option<SharedHelper>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub role: Role,
    pub me: Me,
    pub room: Room,
    pub ui: Vec<Widget>,
    pub error: Option<String>,
    pub running: Vec<String>,
    pub ended: bool,
}

pub type Observer = Arc<dyn Fn(&SessionView) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Service,
    Session,
}

impl Kind {
    fn step(self) -> StepKind {
        match self {
            Kind::Service => StepKind::Service,
            Kind::Session => StepKind::Session,
        }
    }
}

enum Event {
    Program(Kind, FromProgram),
    Exited(Kind, Exit),
    Room(Room),
    HostLost(String),
    Ask {
        member: RoomMember,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    AskExpired(String),
    ServiceExpired,
    Start(oneshot::Sender<Result<(), String>>),
    Action(String),
    Stop(oneshot::Sender<()>),
}

type Events = mpsc::UnboundedSender<Event>;

struct HostHooks {
    events: Events,
    verify: Verify,
}

impl RoomHooks for HostHooks {
    fn changed(&self, room: &Room) {
        let _ = self.events.send(Event::Room(room.clone()));
    }

    fn ask_join(&self, member: &RoomMember) -> Result<(), String> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.events
            .send(Event::Ask {
                member: member.clone(),
                reply,
            })
            .map_err(|_| "the room is closing".to_string())?;
        answer
            .recv_timeout(crate::constants::CARTRIDGE_JOIN_ASK_TIMEOUT)
            .unwrap_or_else(|_| Err("the host's game did not answer".to_string()))
    }

    fn verify(&self, member_id: &str, address: IpAddr) -> bool {
        (self.verify)(member_id, address)
    }
}

enum Link {
    Host {
        room: Arc<RoomHost>,
        beacon: Option<Beacon>,
    },
    Member {
        client: RoomClient,
        updates: watch::Sender<Option<StateUpdate>>,
    },
}

struct Actor {
    role: Role,
    me: Me,
    prepared: Prepared,
    privileges: Privileges,
    log_dir: PathBuf,
    service_address: String,
    link: Link,
    events: Events,
    tree: TaskTree,
    programs: BTreeMap<Kind, Supervised>,
    room: Room,
    relayed_members: Vec<Member>,
    relayed_public: BTreeMap<String, Value>,
    own: StateUpdate,
    ui: Vec<Widget>,
    error: Option<String>,
    ended: bool,
    asks: BTreeMap<String, std::sync::mpsc::Sender<Result<(), String>>>,
    view: Arc<Mutex<SessionView>>,
    observer: Observer,
    ended_tx: watch::Sender<bool>,
}

fn members(room: &Room) -> Vec<Member> {
    room.members
        .iter()
        .map(|member| Member {
            id: member.id.clone(),
            name: member.name.clone(),
            address: member.address.clone(),
        })
        .collect()
}

impl Actor {
    fn label(&self, kind: Kind) -> String {
        format!("{}[{}]", self.prepared.manifest.id, kind.step().as_str())
    }

    fn session_start(&self, kind: Kind) -> SessionStart {
        let wants_service = self.prepared.manifest.lifecycle.service.is_some();
        SessionStart {
            role: if kind == Kind::Service {
                Role::Host
            } else {
                self.role
            },
            me: protocol::Me {
                id: self.me.id.clone(),
                name: self.me.name.clone(),
            },
            members: members(&self.room),
            service: wants_service.then(|| Service {
                address: self.service_address.clone(),
            }),
            settings: self.prepared.settings.clone(),
            paths: Paths {
                cartridge: self.prepared.cartridge_dir.to_string_lossy().into_owned(),
                data: self.prepared.data_dir.to_string_lossy().into_owned(),
            },
            platform: self.prepared.platform,
        }
    }

    fn launch(&mut self, kind: Kind) -> crate::error::Result<()> {
        if self.programs.contains_key(&kind) || self.ended {
            return Ok(());
        }
        let spec = self
            .prepared
            .context()
            .program(kind.step())?
            .ok_or_else(|| format!("the cartridge has no {} program", kind.step().as_str()))?;
        let label = self.label(kind);
        let log = self.log_dir.join(format!(
            "cartridge-{}-{}.log",
            self.prepared.manifest.id,
            kind.step().as_str()
        ));
        let events = self.events.clone();
        let line_label = label.clone();
        let observer: crate::logging::LineObserver = Arc::new(move |line: &str| {
            match protocol::decode::<FromProgram>(line) {
                Line::Message(message) => {
                    let _ = events.send(Event::Program(kind, message));
                }
                Line::Text(text) => log::info!(target: LOG_TARGET, "{line_label} {text}"),
                Line::Invalid { line, error } => {
                    log::warn!(target: LOG_TARGET, "{line_label} sent an invalid line ({error}): {line}")
                }
            }
        });
        let capture = Capture {
            label: label.clone(),
            sink: crate::logging::open_log(&log)?,
            observer: None,
            stdout: Some(observer),
        };
        let child = supervisor::spawn_with(&spec, Some(capture), Stdin::Piped)?;
        log::info!(target: LOG_TARGET, "started {label} (pid {})", child.pid());
        child.send_line(&protocol::encode(&ToProgram::SessionStart(
            self.session_start(kind),
        )));
        let exited = child.exited();
        let events = self.events.clone();
        self.tree.spawn(async move {
            let exit = exited.await;
            let _ = events.send(Event::Exited(kind, exit));
        });
        self.programs.insert(kind, child);
        if kind == Kind::Session {
            self.relayed_members = members(&self.room);
            self.relayed_public.clear();
            self.relay();
        }
        Ok(())
    }

    fn send(&self, message: &ToProgram) {
        let line = protocol::encode(message);
        for child in self.programs.values() {
            child.send_line(&line);
        }
    }

    fn relay(&mut self) {
        if self.programs.is_empty() {
            return;
        }
        let current = members(&self.room);
        if current != self.relayed_members {
            self.relayed_members = current.clone();
            self.send(&ToProgram::RoomChanged { members: current });
        }
        let departed: Vec<String> = self
            .relayed_public
            .keys()
            .filter(|id| self.room.member(id).is_none())
            .cloned()
            .collect();
        for id in departed {
            self.relayed_public.remove(&id);
        }
        let changed: Vec<(String, Value)> = self
            .room
            .members
            .iter()
            .filter(|member| member.id != self.me.id && !member.public.is_null())
            .filter(|member| self.relayed_public.get(&member.id) != Some(&member.public))
            .map(|member| (member.id.clone(), member.public.clone()))
            .collect();
        for (member_id, state) in changed {
            self.relayed_public.insert(member_id.clone(), state.clone());
            self.send(&ToProgram::MemberState { member_id, state });
        }
    }

    fn publish_own(&mut self) {
        match &self.link {
            Link::Host { room, .. } => {
                room.apply(&self.own);
            }
            Link::Member { updates, .. } => {
                updates.send_replace(Some(self.own.clone()));
            }
        }
    }

    fn set_status(&mut self, status: Status, message: Option<String>) {
        self.own.status = Some(status);
        self.own.message = message;
        self.publish_own();
    }

    fn refresh(&mut self) {
        let view = SessionView {
            role: self.role,
            me: self.me.clone(),
            room: self.room.clone(),
            ui: self.ui.clone(),
            error: self.error.clone(),
            running: self
                .programs
                .keys()
                .map(|kind| kind.step().as_str().to_string())
                .collect(),
            ended: self.ended,
        };
        let changed = {
            let mut current = self.view.lock_or_recover();
            let changed = *current != view;
            *current = view.clone();
            changed
        };
        if changed {
            (self.observer)(&view);
        }
    }

    fn on_room(&mut self, room: Room) {
        if room.revision < self.room.revision && room.phase != Phase::Closed {
            return;
        }
        self.room = room;
        if self.role == Role::Member {
            if self.room.phase == Phase::Closed {
                return;
            }
            let service_ok = self.prepared.manifest.lifecycle.service.is_none()
                || self.room.service.is_some_and(|service| service.ready);
            if self.room.phase == Phase::Running
                && service_ok
                && !self.programs.contains_key(&Kind::Session)
            {
                self.set_status(Status::Starting, None);
                let launched = match self.privileges.apply(&self.service_address) {
                    Ok(()) => self.launch(Kind::Session).map_err(|err| err.to_string()),
                    Err(err) => Err(err),
                };
                if let Err(err) = launched {
                    self.fail(err);
                }
            }
        }
        self.relay();
    }

    fn fail(&mut self, message: String) {
        log::warn!(target: LOG_TARGET, "{}: {message}", self.prepared.manifest.id);
        self.error = Some(message.clone());
        self.set_status(Status::Error, Some(message));
        self.ended = true;
    }

    fn start(&mut self) -> Result<(), String> {
        let Link::Host { room, .. } = &self.link else {
            return Err("only the host starts the session".to_string());
        };
        if self.room.phase != Phase::Gathering {
            return Err("the session has already started".to_string());
        }
        self.prepared.preflight().map_err(|err| err.to_string())?;
        self.privileges.apply(&self.service_address)?;
        let has_service = self.prepared.manifest.lifecycle.service.is_some();
        room.update(|room| {
            room.phase = Phase::Running;
            room.service = has_service.then_some(ServiceState { ready: false });
        });
        self.room = room.snapshot();
        self.set_status(Status::Starting, None);
        let kind = if has_service {
            let events = self.events.clone();
            self.tree.spawn(async move {
                tokio::time::sleep(crate::constants::CARTRIDGE_READY_TIMEOUT).await;
                let _ = events.send(Event::ServiceExpired);
            });
            Kind::Service
        } else {
            Kind::Session
        };
        self.launch(kind).map_err(|err| err.to_string())
    }

    fn on_program(&mut self, kind: Kind, message: FromProgram) {
        match message {
            FromProgram::Ready {} => match kind {
                Kind::Service => {
                    if let Link::Host { room, .. } = &self.link {
                        room.update(|room| room.service = Some(ServiceState { ready: true }));
                        self.room = room.snapshot();
                    }
                    if let Err(err) = self.launch(Kind::Session) {
                        self.fail(err.to_string());
                    }
                }
                Kind::Session => self.set_status(Status::Ready, None),
            },
            FromProgram::State { public, ui } => {
                if kind == Kind::Session {
                    self.ui = ui;
                    self.own.public = Some(public);
                    self.publish_own();
                }
            }
            FromProgram::JoinAnswer {
                member_id,
                allow,
                reason,
            } => {
                if let Some(reply) = self.asks.remove(&member_id) {
                    let _ = reply.send(if allow {
                        Ok(())
                    } else {
                        Err(reason.unwrap_or_else(|| "the host's game said no".to_string()))
                    });
                }
            }
            FromProgram::Error { message, fatal } => {
                let label = self.label(kind);
                log::warn!(target: LOG_TARGET, "{label} error: {message}");
                if fatal {
                    self.fail(message);
                } else if kind == Kind::Session {
                    let status = self.own.status.unwrap_or(Status::Starting);
                    self.set_status(status, Some(message));
                }
            }
            FromProgram::Log { level, message } => {
                let level = match level {
                    Level::Error => log::Level::Error,
                    Level::Warn => log::Level::Warn,
                    Level::Info => log::Level::Info,
                    Level::Debug => log::Level::Debug,
                };
                log::log!(target: LOG_TARGET, level, "{} {message}", self.label(kind));
            }
        }
    }

    fn on_exit(&mut self, kind: Kind, exit: Exit) {
        if self.programs.remove(&kind).is_none() || self.ended {
            return;
        }
        let what = match kind {
            Kind::Service => "the shared service",
            Kind::Session => "the game",
        };
        let message = match exit.code {
            Some(0) if kind == Kind::Session => None,
            Some(code) => Some(format!("{what} exited with code {code}")),
            None => Some(format!("{what} was killed")),
        };
        match message {
            Some(message) => self.fail(message),
            None => {
                self.set_status(Status::Exited, None);
                self.ended = true;
            }
        }
    }

    fn on_ask(&mut self, member: RoomMember, reply: std::sync::mpsc::Sender<Result<(), String>>) {
        if !self.programs.contains_key(&Kind::Session) {
            let _ = reply.send(Err("the game is not running yet".to_string()));
            return;
        }
        self.send(&ToProgram::JoinRequest {
            member: Member {
                id: member.id.clone(),
                name: member.name.clone(),
                address: member.address.clone(),
            },
        });
        let events = self.events.clone();
        let id = member.id.clone();
        self.tree.spawn(async move {
            tokio::time::sleep(crate::constants::CARTRIDGE_JOIN_ASK_TIMEOUT).await;
            let _ = events.send(Event::AskExpired(id));
        });
        self.asks.insert(member.id, reply);
    }

    async fn shutdown(&mut self) {
        let farewell = protocol::encode(&ToProgram::Stop {});
        let programs = std::mem::take(&mut self.programs);
        for kind in [Kind::Session, Kind::Service] {
            if let Some(child) = programs.get(&kind) {
                let exit = child
                    .stop(crate::constants::CARTRIDGE_STOP_GRACE, Some(&farewell))
                    .await;
                log::info!(target: LOG_TARGET, "stopped {} ({exit:?})", self.label(kind));
            }
        }
        self.privileges.release();
        for (_, reply) in std::mem::take(&mut self.asks) {
            let _ = reply.send(Err("the room is closing".to_string()));
        }
        match &mut self.link {
            Link::Host { room, beacon } => {
                room.close(self.error.clone());
                self.room = room.snapshot();
                tokio::time::sleep(crate::constants::ROOM_CLOSE_LINGER).await;
                if let Some(beacon) = beacon.take() {
                    tasks::blocking(move || beacon.stop()).await;
                }
            }
            Link::Member { client, .. } => {
                let client = client.clone();
                let id = self.me.id.clone();
                if self.room.phase != Phase::Closed {
                    tasks::blocking(move || client.leave(&id)).await;
                }
            }
        }
        self.tree.cancel();
    }

    async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Event>) {
        self.refresh();
        while !self.ended {
            let Some(event) = inbox.recv().await else {
                break;
            };
            let mut stopped = None;
            match event {
                Event::Program(kind, message) => self.on_program(kind, message),
                Event::Exited(kind, exit) => self.on_exit(kind, exit),
                Event::Room(room) => {
                    let closed = room.phase == Phase::Closed;
                    let message = room.message.clone();
                    self.on_room(room);
                    if closed && self.role == Role::Member {
                        self.error =
                            message.or_else(|| Some("the host closed the room".to_string()));
                        self.ended = true;
                    }
                }
                Event::HostLost(message) => {
                    self.error = Some(format!("lost the host: {message}"));
                    self.ended = true;
                }
                Event::Ask { member, reply } => self.on_ask(member, reply),
                Event::AskExpired(id) => {
                    if let Some(reply) = self.asks.remove(&id) {
                        let _ = reply.send(Err("the host's game did not answer".to_string()));
                    }
                }
                Event::ServiceExpired => {
                    if self.room.service.is_some_and(|service| !service.ready) {
                        self.fail("the shared service did not get ready in time".to_string());
                    }
                }
                Event::Start(reply) => {
                    let result = self.start();
                    if let Err(err) = &result {
                        if self.programs.is_empty() && self.room.phase == Phase::Running {
                            self.fail(err.clone());
                        }
                    }
                    let _ = reply.send(result);
                }
                Event::Action(id) => self.send(&ToProgram::Action { id }),
                Event::Stop(done) => {
                    self.ended = true;
                    stopped = Some(done);
                }
            }
            if self.ended {
                self.shutdown().await;
            }
            self.refresh();
            if let Some(done) = stopped {
                let _ = done.send(());
            }
        }
        while let Ok(event) = inbox.try_recv() {
            match event {
                Event::Stop(done) => {
                    let _ = done.send(());
                }
                Event::Ask { reply, .. } => {
                    let _ = reply.send(Err("the room is closed".to_string()));
                }
                Event::Start(reply) => {
                    let _ = reply.send(Err("the session has ended".to_string()));
                }
                _ => {}
            }
        }
        let _ = self.ended_tx.send(true);
    }
}

pub struct Session {
    addresses: Vec<std::net::SocketAddr>,
    events: Events,
    view: Arc<Mutex<SessionView>>,
    ended: watch::Receiver<bool>,
    cartridge: CartridgeRef,
    digest: String,
}

struct Parts {
    events: Events,
    inbox: mpsc::UnboundedReceiver<Event>,
    view: Arc<Mutex<SessionView>>,
    ended_tx: watch::Sender<bool>,
    ended: watch::Receiver<bool>,
}

fn parts(role: Role, me: &Me, room: &Room) -> Parts {
    let (events, inbox) = mpsc::unbounded_channel();
    let (ended_tx, ended) = watch::channel(false);
    let view = Arc::new(Mutex::new(SessionView {
        role,
        me: me.clone(),
        room: room.clone(),
        ui: Vec::new(),
        error: None,
        running: Vec::new(),
        ended: false,
    }));
    Parts {
        events,
        inbox,
        view,
        ended_tx,
        ended,
    }
}

impl Session {
    pub fn host(
        prepared: Prepared,
        me: Me,
        options: HostOptions,
        observer: Observer,
    ) -> crate::error::Result<Session> {
        prepared.preflight()?;
        let privileges = prepared.privileges(options.helper);
        privileges.check()?;
        let room = Room {
            protocol: PROTOCOL,
            room_id: format!("{}-{}", me.id, crate::time::now_ms()),
            host: me.id.clone(),
            cartridge: prepared.room_cartridge(),
            max_members: prepared.manifest.room.max_members,
            late_join: prepared.manifest.room.late_join,
            phase: Phase::Gathering,
            service: None,
            members: vec![RoomMember {
                id: me.id.clone(),
                name: me.name.clone(),
                address: options.address.clone(),
                status: Status::Joined,
                message: None,
                public: Value::Null,
            }],
            message: None,
            revision: 0,
        };
        let parts = parts(Role::Host, &me, &room);
        let hooks = Arc::new(HostHooks {
            events: parts.events.clone(),
            verify: options.verify,
        });
        let host = Arc::new(RoomHost::new(room.clone(), hooks));
        let handler = {
            let host = Arc::clone(&host);
            let version = options.app_version.clone();
            Arc::new(move |request| host.handle(request, &version))
        };
        let beacon = Beacon::serve(&options.bind, options.port, handler)?;
        let session = Session {
            addresses: beacon.local_addresses(),
            events: parts.events.clone(),
            view: Arc::clone(&parts.view),
            ended: parts.ended,
            cartridge: prepared.cartridge.clone(),
            digest: prepared.digest.clone(),
        };
        let actor = Actor {
            role: Role::Host,
            own: StateUpdate {
                member_id: me.id.clone(),
                status: Some(Status::Joined),
                message: None,
                public: None,
            },
            me,
            prepared,
            privileges,
            log_dir: options.log_dir,
            service_address: "127.0.0.1".to_string(),
            link: Link::Host {
                room: host,
                beacon: Some(beacon),
            },
            events: parts.events,
            tree: TaskTree::default(),
            programs: BTreeMap::new(),
            room,
            relayed_members: Vec::new(),
            relayed_public: BTreeMap::new(),
            ui: Vec::new(),
            error: None,
            ended: false,
            asks: BTreeMap::new(),
            view: parts.view,
            observer,
            ended_tx: parts.ended_tx,
        };
        tauri::async_runtime::spawn(actor.run(parts.inbox));
        Ok(session)
    }

    pub fn join(
        prepare: impl FnOnce(&RoomCartridge) -> crate::error::Result<Prepared>,
        me: Me,
        options: JoinOptions,
        observer: Observer,
    ) -> crate::error::Result<Session> {
        let client = RoomClient::new(&options.host, options.port);
        client.hello()?;
        let preview = client.room(None)?;
        let prepared = prepare(&preview.cartridge)?;
        prepared.preflight()?;
        let privileges = prepared.privileges(options.helper);
        privileges.check()?;
        let room = client.join(&JoinRequest {
            member: Joiner {
                id: me.id.clone(),
                name: me.name.clone(),
            },
            digest: prepared.digest.clone(),
        })?;
        let parts = parts(Role::Member, &me, &room);
        let (updates, mut pending) = watch::channel::<Option<StateUpdate>>(None);
        let tree = TaskTree::default();
        {
            let client = client.clone();
            tree.spawn(async move {
                while pending.changed().await.is_ok() {
                    let Some(update) = pending.borrow_and_update().clone() else {
                        continue;
                    };
                    let client = client.clone();
                    let sent = tasks::blocking(move || client.update(&update)).await;
                    if let Some(Err(err)) = sent {
                        log::warn!(target: LOG_TARGET, "cannot publish to the host: {err}");
                    }
                }
            });
        }
        {
            let client = client.clone();
            let events = parts.events.clone();
            let mut revision = room.revision;
            tree.spawn(async move {
                let mut lost_since: Option<std::time::Instant> = None;
                loop {
                    let polling = client.clone();
                    let polled = tasks::blocking(move || polling.room(Some(revision))).await;
                    match polled {
                        Some(Ok(room)) => {
                            lost_since = None;
                            revision = room.revision;
                            let closed = room.phase == Phase::Closed;
                            let _ = events.send(Event::Room(room));
                            if closed {
                                break;
                            }
                        }
                        Some(Err(err)) => {
                            let since = *lost_since.get_or_insert_with(std::time::Instant::now);
                            if since.elapsed() >= crate::constants::ROOM_HOST_LOST_AFTER {
                                let _ = events.send(Event::HostLost(err));
                                break;
                            }
                            tokio::time::sleep(crate::constants::ROOM_RETRY_INTERVAL).await;
                        }
                        None => break,
                    }
                }
            });
        }
        let session = Session {
            addresses: Vec::new(),
            events: parts.events.clone(),
            view: Arc::clone(&parts.view),
            ended: parts.ended,
            cartridge: prepared.cartridge.clone(),
            digest: prepared.digest.clone(),
        };
        let actor = Actor {
            role: Role::Member,
            own: StateUpdate {
                member_id: me.id.clone(),
                status: Some(Status::Joined),
                message: None,
                public: None,
            },
            me,
            prepared,
            privileges,
            log_dir: options.log_dir,
            service_address: options.host.clone(),
            link: Link::Member { client, updates },
            events: parts.events.clone(),
            tree,
            programs: BTreeMap::new(),
            room: room.clone(),
            relayed_members: Vec::new(),
            relayed_public: BTreeMap::new(),
            ui: Vec::new(),
            error: None,
            ended: false,
            asks: BTreeMap::new(),
            view: parts.view,
            observer,
            ended_tx: parts.ended_tx,
        };
        tauri::async_runtime::spawn(actor.run(parts.inbox));
        let _ = parts.events.send(Event::Room(room));
        Ok(session)
    }

    pub fn addresses(&self) -> &[std::net::SocketAddr] {
        &self.addresses
    }

    pub fn cartridge(&self) -> &CartridgeRef {
        &self.cartridge
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn view(&self) -> SessionView {
        self.view.lock_or_recover().clone()
    }

    pub fn is_ended(&self) -> bool {
        *self.ended.borrow()
    }

    pub async fn start(&self) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(Event::Start(reply))
            .map_err(|_| "the session has ended".to_string())?;
        answer
            .await
            .unwrap_or_else(|_| Err("the session has ended".to_string()))
    }

    pub fn action(&self, id: &str) {
        let _ = self.events.send(Event::Action(id.to_string()));
    }

    pub async fn stop(&self) {
        let (done, finished) = oneshot::channel();
        if self.events.send(Event::Stop(done)).is_ok() {
            let _ = finished.await;
        }
        self.ended().await;
    }

    pub async fn ended(&self) {
        let mut ended = self.ended.clone();
        let _ = ended.wait_for(|ended| *ended).await;
    }
}

#[cfg(test)]
mod tests;
