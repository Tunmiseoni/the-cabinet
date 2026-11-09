use crate::commands::config::{config_and_provider, load_config};
use crate::commands::launch::{launch_match_inner, LaunchRequest};
use crate::config::Config;
use crate::constants;
use crate::lobby::beacon;
use crate::lobby::results::{self, HealthSnapshot, RomAddresses, RoundWatcher};
use crate::lobby::room::{new_room_id, Room};
use crate::lobby::sets::{winner_slot, SetEvent, SetMachine};
use crate::lobby::Lobby;
use crate::net::Node;
use crate::providers::Role;
use crate::session::MatchState;
use crate::tasks;
use serde::{Deserialize, Serialize};
use std::ops::ControlFlow;
use tauri::{AppHandle, Manager};

const DEFAULT_FIRST_TO: u8 = 2;

fn default_first_to() -> u8 {
    DEFAULT_FIRST_TO
}

fn beacon_port(cfg: &Config) -> u16 {
    if cfg.lobby_beacon_port == 0 {
        constants::LOBBY_BEACON_PORT
    } else {
        cfg.lobby_beacon_port
    }
}

fn self_node(cfg: &Config) -> Option<Node> {
    crate::net::provider(cfg)
        .ok()
        .and_then(|network| network.self_node().ok().flatten())
}

fn host_identity(cfg: &Config) -> (String, String) {
    let self_peer = self_node(cfg);
    let node_id = self_peer
        .as_ref()
        .and_then(crate::net::local_identity)
        .map(|identity| identity.member_id())
        .or_else(crate::env::os_hostname)
        .unwrap_or_else(|| "unknown".to_string());
    // The displayed handle is the same identity the lobby seats by, so the room and its seats agree.
    (
        node_id,
        crate::config::netplay_nickname(cfg, self_peer.as_ref().map(|peer| peer.hostname.as_str())),
    )
}

/// This machine's own netplay nickname — the identity the lobby keys seats by.
fn netplay_identity(cfg: &Config) -> String {
    let tailnet = self_node(cfg).map(|peer| peer.hostname);
    crate::config::netplay_nickname(cfg, tailnet.as_deref())
}

/// Resolve the launch role and seat for a joiner. A player always connects as a client
/// (`Role::P2`) and lets RetroArch hand it the first free player device; the room's advertised
/// seats decide whether a slot is free. A full room joins as a spectator rather than erroring —
/// the set-end rotation promotes the longest-waiting spectator when a seat opens.
fn assign_join(
    spectate: bool,
    requested: Option<u8>,
    room: Option<&Room>,
) -> crate::error::Result<(Role, Option<u8>)> {
    if spectate {
        return Ok((Role::Spectator, None));
    }
    if let Some(slot) = requested {
        if !(1..=2).contains(&slot) {
            return Err(format!("invalid player slot {slot}: expected 1 or 2").into());
        }
        return Ok((Role::P2, Some(slot)));
    }
    let Some(room) = room else {
        // No beacon answered (a manual join by address), so occupancy is unknown. Assume the
        // room is empty; a host that is playing player 1 will refuse the duplicate seat.
        return Ok((Role::P2, Some(1)));
    };
    match room.seats.iter().position(|slot| slot.is_none()) {
        Some(index) => Ok((Role::P2, Some(index as u8 + 1))),
        None => Ok((Role::Spectator, None)),
    }
}

/// The command port of this machine's own instance. The host is always `Role::P1`; a joiner is
/// either `Role::P2` or `Role::Spectator`.
fn own_command_port(app: &AppHandle, host: bool) -> Option<u16> {
    crate::session::status(app)
        .instances
        .iter()
        .find(|instance| (instance.role == Role::P1) == host)
        .and_then(|instance| instance.command_port)
}

fn session_running(app: &AppHandle) -> bool {
    crate::session::status(app).status == "running"
}

/// The incoming player's seat control byte says whether the game has seen their coin yet:
/// `Some(nick)` while the rotation's incoming holds the losing seat but the game still reads that
/// seat as game-driven (they have not coined in), and `None` once the coin flips it human — or
/// while the handover is still pending, when the seat is not yet theirs. The host publishes it on
/// the beacon so every machine's UI can prompt the right player (docs/10-lobby-spike.md §6 L16).
fn awaiting_coin(room: &Room, snapshot: &HealthSnapshot) -> Option<String> {
    let rotation = room.rotation.as_ref()?;
    let incoming = rotation.incoming.as_deref()?;
    let seat = rotation.loser_slot.checked_sub(1)? as usize;
    if room.seats.get(seat)?.as_deref() != Some(incoming) {
        return None;
    }
    let control = if rotation.loser_slot == 2 {
        snapshot.p2_control
    } else {
        snapshot.p1_control
    };
    (control != results::CONTROL_HUMAN).then(|| incoming.to_string())
}

/// Per-machine rotation bookkeeping: the last rotation acted on (or primed past), whether the
/// first room snapshot has been seen, and whether the wait for the losing seat to free has been
/// logged for the current rotation.
#[derive(Debug, Default)]
struct RotationState {
    last: Option<u64>,
    primed: bool,
    waiting_logged: bool,
}

/// Toggle this machine's own instance once per advertised rotation: the loser steps out, the
/// queue head steps in. The first room seen only primes the state, so an in-flight rotation at
/// join time is never acted on twice.
///
/// The incoming side waits until the room reports the losing seat actually free. A request sent
/// while the loser still holds the seat is granted the next free *input device* (device 2,
/// announced as "player 3"), not the losing seat, and that instance is then invisible to both the
/// game and the seat map (docs/10-lobby-spike.md §6 L19).
fn reconcile_rotation(
    state: &mut RotationState,
    room: &Room,
    own_nick: &str,
    current_slot: &mut Option<u8>,
    command_port: u16,
) {
    let Some(rotation) = &room.rotation else {
        state.primed = true;
        return;
    };
    if !state.primed {
        state.last = Some(rotation.id);
        state.primed = true;
        return;
    }
    if state.last == Some(rotation.id) {
        return;
    }

    let is_incoming = rotation.incoming.as_deref() == Some(own_nick);
    let is_loser = *current_slot == Some(rotation.loser_slot);
    if !is_incoming && !is_loser {
        state.last = Some(rotation.id);
        return;
    }

    if is_incoming {
        let seats = room.seats();
        let occupant = rotation
            .loser_slot
            .checked_sub(1)
            .and_then(|index| seats.slots.get(index as usize))
            .and_then(|entry| entry.clone());
        match occupant.as_deref() {
            Some(nick) if nick == own_nick => {
                // Already in the seat (a previous poll acted, or the observation lagged): done.
                state.last = Some(rotation.id);
                return;
            }
            Some(_) => {
                // Retry on the next poll: `state.last` stays unmarked until we actually act.
                if !state.waiting_logged {
                    log::info!(
                        "rotation {}: waiting for seat {} to free before stepping in",
                        rotation.id,
                        rotation.loser_slot
                    );
                    state.waiting_logged = true;
                }
                return;
            }
            None => {}
        }
    }

    match crate::providers::command::toggle_game_watch(command_port) {
        Ok(()) => {
            log::info!(
                "rotation {}: {own_nick} {}",
                rotation.id,
                if is_loser { "steps out" } else { "steps in" }
            );
            *current_slot = if is_loser {
                None
            } else {
                Some(rotation.loser_slot)
            };
            state.last = Some(rotation.id);
            state.waiting_logged = false;
        }
        Err(err) => log::warn!("cannot act on rotation {}: {err}", rotation.id),
    }
}

/// Host-side set machine: poll the host instance's RAM, roll rounds up to games and sets, publish
/// the score, and announce a rotation when a set ends. Only ROMs with a known address table (see
/// `lobby::results`) are watched, so unsupported ROMs still play but do not rotate automatically.
fn spawn_set_runner(
    app: AppHandle,
    room_id: String,
    rom: String,
    first_to: u8,
    own_nick: String,
    initial_slot: Option<u8>,
) {
    let Some(addresses) = results::addresses_for(&rom) else {
        log::info!("no result addresses for {rom}; automatic rotation is off for this room");
        return;
    };
    let runner = SetRunner {
        app: app.clone(),
        room_id,
        rom,
        addresses,
        own_nick,
        watcher: RoundWatcher::new(),
        machine: SetMachine::new(first_to),
        current_slot: initial_slot,
        rotation_state: RotationState::default(),
        ticks: 0,
    };
    crate::session::spawn_task(
        &app,
        tasks::poll_blocking(constants::LOBBY_SET_POLL, runner, SetRunner::tick),
    );
}

struct SetRunner {
    app: AppHandle,
    room_id: String,
    rom: String,
    addresses: RomAddresses,
    own_nick: String,
    watcher: RoundWatcher,
    machine: SetMachine,
    current_slot: Option<u8>,
    rotation_state: RotationState,
    ticks: u64,
}

impl SetRunner {
    fn tick(&mut self) -> ControlFlow<()> {
        if !session_running(&self.app) {
            return ControlFlow::Break(());
        }
        let Some(lobby) = self.app.try_state::<Lobby>() else {
            return ControlFlow::Break(());
        };
        let Some(room) = lobby.room() else {
            return ControlFlow::Break(());
        };
        if room.room_id != self.room_id {
            return ControlFlow::Break(());
        }
        let Some(port) = own_command_port(&self.app, true) else {
            return ControlFlow::Continue(());
        };
        let ticks = self.ticks;
        if ticks == 0 {
            log::info!(
                "set runner: watching {} on command port {port} ({:?})",
                self.rom,
                self.addresses
            );
        }
        // The watcher only emits inside a round the game reported live and says P2 is
        // human-controlled, so select/continue/idle screens, a lone host's arcade play, the
        // arcade drift after a 2P match, and the attract demo can no longer decide a round
        // (docs/10-lobby-spike.md §6 L12/L15).
        match results::read_snapshot(port, self.addresses) {
            Ok(snapshot) => {
                if ticks.is_multiple_of(20) {
                    log::info!("set runner snapshot #{ticks}: {snapshot:?}");
                }
                if let Some(outcome) = self.watcher.observe(snapshot) {
                    log::info!("set runner outcome: {outcome:?}");
                    let event = self.machine.observe(outcome);
                    lobby.set_score(self.machine.score());
                    if let SetEvent::SetWon(winner) = event {
                        if let Some(slot) = winner_slot(winner) {
                            if let Some(rotation) = lobby.announce_rotation(slot) {
                                log::info!(
                                    "set won in slot {slot}; rotation {} -> {:?}",
                                    rotation.id,
                                    rotation.incoming
                                );
                            }
                        }
                    }
                }
                // Publish whether the rotation's incoming still owes the game a coin, so the
                // UI can prompt them (L16). Clears as soon as the game reads the seat human.
                if let Some(room) = lobby.room() {
                    let awaiting = awaiting_coin(&room, &snapshot);
                    if awaiting != room.awaiting_coin {
                        log::info!("awaiting coin: {awaiting:?}");
                    }
                    lobby.set_awaiting_coin(awaiting);
                }
            }
            Err(err) => log::debug!("set watcher read failed: {err}"),
        }
        if let Some(room) = lobby.room() {
            reconcile_rotation(
                &mut self.rotation_state,
                &room,
                &self.own_nick,
                &mut self.current_slot,
                port,
            );
        }
        self.ticks += 1;
        ControlFlow::Continue(())
    }
}

/// Joiner-side seat follower: poll the host's beacon and act on the advertised rotation, so a
/// spectator is promoted into a freed seat and a losing player steps out without a cross-network
/// control channel.
fn spawn_seat_follower(app: AppHandle, host: String, own_nick: String, initial_slot: Option<u8>) {
    let follower = SeatFollower {
        app: app.clone(),
        host,
        own_nick,
        current_slot: initial_slot,
        rotation_state: RotationState::default(),
    };
    crate::session::spawn_task(
        &app,
        tasks::poll_blocking(constants::LOBBY_SEAT_POLL, follower, SeatFollower::tick),
    );
}

struct SeatFollower {
    app: AppHandle,
    host: String,
    own_nick: String,
    current_slot: Option<u8>,
    rotation_state: RotationState,
}

impl SeatFollower {
    fn tick(&mut self) -> ControlFlow<()> {
        if !session_running(&self.app) {
            return ControlFlow::Break(());
        }
        let Some(port) = own_command_port(&self.app, false) else {
            return ControlFlow::Continue(());
        };
        let Ok(cfg) = load_config(&self.app) else {
            return ControlFlow::Continue(());
        };
        let Some(room) = beacon::query(
            &self.host,
            beacon_port(&cfg),
            constants::LOBBY_BEACON_QUERY_TIMEOUT,
        ) else {
            return ControlFlow::Continue(());
        };
        reconcile_rotation(
            &mut self.rotation_state,
            &room,
            &self.own_nick,
            &mut self.current_slot,
            port,
        );
        ControlFlow::Continue(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LobbyStartRequest {
    pub rom: String,
    #[serde(default = "default_first_to")]
    pub first_to: u8,
    /// The seat the host itself takes (`1`/`2`), or `None` to host as a non-playing spectator
    /// (the "table"). Defaults to seat 1 so a one-friend session is host-vs-joiner.
    #[serde(default)]
    pub host_seat: Option<u8>,
}

#[tauri::command(async)]
pub fn lobby_start(
    app: AppHandle,
    request: LobbyStartRequest,
) -> crate::error::CommandResult<Room> {
    let (cfg, provider) = config_and_provider(&app, false)?;
    let lobby = app.state::<Lobby>();
    if lobby.room().is_some() {
        return Err("a room is already hosting".into());
    }
    let install = provider.detect()?;
    if !install.installed {
        return Err(format!("emulator not found — {}", install.detail).into());
    }
    let rom = request.rom.trim().to_string();
    if rom.is_empty() {
        return Err("choose a ROM for the room".into());
    }
    let first_to = request.first_to.clamp(1, 9);
    let host_seat = match request.host_seat {
        Some(slot) if (1..=2).contains(&slot) => Some(slot),
        Some(slot) => {
            return Err(format!("invalid host seat {slot}: expected 1 or 2").into());
        }
        None => None,
    };
    let (node_id, handle) = host_identity(&cfg);
    let own_nick = netplay_identity(&cfg);
    let mut room = Room::new(
        new_room_id(),
        node_id,
        handle,
        rom.clone(),
        first_to,
        host_seat,
    );
    if let Some(seat) = host_seat {
        let mut seats = room.seats();
        seats.set(seat, Some(own_nick.clone()));
        room.set_seats(&seats);
    }
    let room = lobby.start(room, beacon_port(&cfg), own_nick.clone())?;

    log::info!(
        "hosting room {} ({} first to {first_to}, host seat {host_seat:?})",
        room.room_id,
        room.rom
    );
    if let Err(err) = launch_match_inner(
        &app,
        LaunchRequest {
            rom: rom.clone(),
            peer_ip: String::new(),
            role: Role::P1,
            player_slot: host_seat,
            dev: false,
            force: false,
            host_spectating: host_seat.is_none(),
        },
    ) {
        lobby.stop();
        return Err(err.into());
    }
    spawn_set_runner(
        app.clone(),
        room.room_id.clone(),
        rom,
        room.first_to,
        own_nick,
        host_seat,
    );
    Ok(room)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LobbyJoinRequest {
    pub host: String,
    #[serde(default)]
    pub rom: Option<String>,
    #[serde(default)]
    pub spectate: bool,
    #[serde(default)]
    pub player_slot: Option<u8>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinOutcome {
    pub room: Option<Room>,
    pub role: Role,
    pub player_slot: Option<u8>,
    pub state: MatchState,
}

#[tauri::command(async)]
pub fn lobby_join(
    app: AppHandle,
    request: LobbyJoinRequest,
) -> crate::error::CommandResult<JoinOutcome> {
    let cfg = load_config(&app)?;
    let host = request.host.trim().to_string();
    if host.is_empty() {
        return Err("enter a host address to join".into());
    }
    let room = beacon::query(
        &host,
        beacon_port(&cfg),
        constants::LOBBY_BEACON_QUERY_TIMEOUT,
    );
    let rom = request
        .rom
        .as_deref()
        .map(str::trim)
        .filter(|rom| !rom.is_empty())
        .map(str::to_string)
        .or_else(|| room.as_ref().map(|room| room.rom.clone()))
        .ok_or_else(|| {
            "no room answered at that address — enter the ROM to join directly".to_string()
        })?;
    let players = room.as_ref().map(|room| room.players).unwrap_or(0);
    let (role, player_slot) = assign_join(request.spectate, request.player_slot, room.as_ref())?;

    log::info!(
        "joining {host} room={:?} rom={rom} role={} seat={player_slot:?} (advertised players {players})",
        room.as_ref().map(|room| room.room_id.as_str()),
        role.label(),
    );
    let own_nick = netplay_identity(&cfg);
    let state = launch_match_inner(
        &app,
        LaunchRequest {
            rom,
            peer_ip: host.clone(),
            role,
            player_slot,
            dev: false,
            force: request.force,
            host_spectating: false,
        },
    )?;
    spawn_seat_follower(app.clone(), host, own_nick, player_slot);
    Ok(JoinOutcome {
        room,
        role,
        player_slot,
        state,
    })
}

#[tauri::command(async)]
pub fn lobby_stop(app: AppHandle) -> crate::error::CommandResult<()> {
    app.state::<Lobby>().stop();
    let _ = crate::session::stop(&app);
    Ok(())
}

#[tauri::command]
pub fn lobby_status(app: AppHandle) -> Option<Room> {
    app.state::<Lobby>().room()
}

/// The room this machine is currently in: the hosted room, or the room of the match it joined
/// (queried from the host's beacon). `None` when idle. The UI polls this for the live seat/score
/// view.
#[tauri::command(async)]
pub fn lobby_room(app: AppHandle) -> crate::error::CommandResult<Option<Room>> {
    let lobby = app.state::<Lobby>();
    if let Some(room) = lobby.room() {
        return Ok(Some(room));
    }
    let state = crate::session::status(&app);
    if state.status != "running" {
        return Ok(None);
    }
    let Some(host) = state
        .peer_ip
        .as_deref()
        .map(str::trim)
        .filter(|ip| !ip.is_empty() && *ip != "127.0.0.1")
    else {
        return Ok(None);
    };
    let cfg = load_config(&app)?;
    Ok(beacon::query(
        host,
        beacon_port(&cfg),
        constants::LOBBY_BEACON_QUERY_TIMEOUT,
    ))
}

#[tauri::command(async)]
pub fn lobby_query(app: AppHandle, host: String) -> crate::error::CommandResult<Option<Room>> {
    let cfg = load_config(&app)?;
    let host = host.trim();
    if host.is_empty() {
        return Err("enter a host address to join".into());
    }
    Ok(beacon::query(
        host,
        beacon_port(&cfg),
        constants::LOBBY_BEACON_QUERY_TIMEOUT,
    ))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredRoom {
    pub host_ip: String,
    pub host_hostname: String,
    pub room: Room,
}

#[tauri::command(async)]
pub fn lobby_discover(app: AppHandle) -> crate::error::CommandResult<Vec<DiscoveredRoom>> {
    let cfg = load_config(&app)?;
    let port = beacon_port(&cfg);
    let Ok(network) = crate::net::provider(&cfg) else {
        return Ok(Vec::new());
    };
    let peers = network.peers()?;
    let mut rooms = Vec::new();
    for peer in peers
        .iter()
        .filter(|peer| peer.online && !peer.is_self && !peer.ip.trim().is_empty())
    {
        if let Some(room) = beacon::query(&peer.ip, port, constants::LOBBY_BEACON_PROBE_TIMEOUT) {
            rooms.push(DiscoveredRoom {
                host_ip: peer.ip.clone(),
                host_hostname: peer.hostname.clone(),
                room,
            });
        }
    }
    Ok(rooms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lobby::sets::{Rotation, Seats};

    fn room_with(pairs: &[(u8, &str)]) -> Room {
        let mut room = Room::new("r", "node", "player-one", "sfiii3nr1", 2, None);
        let mut seats = Seats::default();
        for (slot, nick) in pairs {
            seats.set(*slot, Some((*nick).to_string()));
        }
        room.set_seats(&seats);
        room
    }

    /// A real `Lobby` hosting on an OS-assigned beacon port, with the given seats/queue.
    fn lobby_with_seats(seats: Seats) -> Lobby {
        let lobby = Lobby::default();
        let room = Room::new("r", "node", "player-one", "sfiii3nr1", 1, None);
        lobby.start(room, 0, "player-one".into()).unwrap();
        lobby.update(|room| room.set_seats(&seats));
        lobby
    }

    fn full_room_with_waiting_watcher() -> Lobby {
        let mut seats = Seats::default();
        seats.set(1, Some("player-one".into()));
        seats.set(2, Some("player-two".into()));
        seats.enqueue("watcher".into());
        lobby_with_seats(seats)
    }

    fn query_live_beacon(lobby: &Lobby) -> Room {
        beacon::query(
            "127.0.0.1",
            lobby.beacon_port().unwrap(),
            constants::LOBBY_BEACON_QUERY_TIMEOUT,
        )
        .unwrap()
    }

    #[test]
    fn the_first_player_joins_on_seat_one() {
        let room = room_with(&[]);
        assert_eq!(
            assign_join(false, None, Some(&room)).unwrap(),
            (Role::P2, Some(1))
        );
    }

    #[test]
    fn the_second_player_joins_on_seat_two() {
        let room = room_with(&[(1, "player-one")]);
        assert_eq!(
            assign_join(false, None, Some(&room)).unwrap(),
            (Role::P2, Some(2))
        );
    }

    #[test]
    fn a_joiner_avoids_a_playing_hosts_seat() {
        assert_eq!(
            assign_join(false, None, Some(&room_with(&[(1, "player-one")]))).unwrap(),
            (Role::P2, Some(2))
        );
        assert_eq!(
            assign_join(false, None, Some(&room_with(&[(2, "player-one")]))).unwrap(),
            (Role::P2, Some(1))
        );
    }

    #[test]
    fn a_full_room_joins_as_a_spectator_instead_of_erroring() {
        let full = room_with(&[(1, "player-one"), (2, "player-two")]);
        assert_eq!(
            assign_join(false, None, Some(&full)).unwrap(),
            (Role::Spectator, None)
        );
        assert_eq!(
            assign_join(true, None, Some(&full)).unwrap(),
            (Role::Spectator, None)
        );
    }

    #[test]
    fn a_manual_join_without_a_beacon_falls_back_to_seat_one() {
        assert_eq!(assign_join(false, None, None).unwrap(), (Role::P2, Some(1)));
    }

    #[test]
    fn an_explicit_seat_wins_over_occupancy() {
        let room = room_with(&[]);
        assert_eq!(
            assign_join(false, Some(2), Some(&room)).unwrap(),
            (Role::P2, Some(2))
        );
    }

    #[test]
    fn an_out_of_range_seat_is_rejected() {
        assert!(assign_join(false, Some(0), None).is_err());
        assert!(assign_join(false, Some(3), None).is_err());
    }

    fn rotation(loser_slot: u8, incoming: &str) -> Room {
        let mut room = room_with(&[(1, "player-one"), (2, "player-two")]);
        room.set_rotation(Rotation {
            id: 3,
            loser_slot,
            incoming: Some(incoming.to_string()),
        });
        room
    }

    /// The room once the loser has stepped out: the losing seat is free and the rotation is still
    /// advertised, which is what the incoming acts on.
    fn freed_rotation(incoming: &str) -> Room {
        let mut room = room_with(&[(1, "player-one")]);
        room.set_rotation(Rotation {
            id: 3,
            loser_slot: 2,
            incoming: Some(incoming.to_string()),
        });
        room
    }

    fn state_primed(last: Option<u64>) -> RotationState {
        RotationState {
            last,
            primed: true,
            waiting_logged: false,
        }
    }

    /// The room once the incoming has taken the losing seat (the handover completed).
    fn seated_rotation(incoming: &str) -> Room {
        let mut room = room_with(&[(1, "player-one"), (2, incoming)]);
        room.set_rotation(Rotation {
            id: 3,
            loser_slot: 2,
            incoming: Some(incoming.to_string()),
        });
        room
    }

    fn snapshot(p1_control: u8, p2_control: u8) -> HealthSnapshot {
        HealthSnapshot {
            p1_health: 0xA0,
            p2_health: 0xA0,
            rounds: 0,
            phase: results::PHASE_LIVE,
            p1_control,
            p2_control,
        }
    }

    /// Clear a seat in a live lobby, as the loser's step-out does through the beacon.
    fn free_seat(lobby: &Lobby, slot: u8) {
        lobby.update(|room| {
            let mut seats = room.seats();
            seats.set(slot, None);
            room.set_seats(&seats);
        });
    }

    #[test]
    fn the_loser_steps_out_and_clears_its_slot() {
        let room = rotation(2, "watcher");
        let mut current = Some(2);
        let mut state = state_primed(Some(2)); // already primed; rotation 3 is new
        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let (len, _) = server.recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..len], b"NETPLAY_GAME_WATCH\n");
        });
        reconcile_rotation(&mut state, &room, "player-two", &mut current, port);
        handle.join().unwrap();
        assert_eq!(current, None);
        assert_eq!(state.last, Some(3));
    }

    #[test]
    fn the_queue_head_steps_in_once_the_losing_seat_is_free() {
        let room = freed_rotation("watcher");
        let mut current = None;
        let mut state = state_primed(Some(2)); // already primed; rotation 3 is new
        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let (len, _) = server.recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..len], b"NETPLAY_GAME_WATCH\n");
        });
        reconcile_rotation(&mut state, &room, "watcher", &mut current, port);
        handle.join().unwrap();
        assert_eq!(current, Some(2));
        assert_eq!(state.last, Some(3));
    }

    #[test]
    fn the_queue_head_waits_while_the_losing_seat_is_occupied() {
        // The rotation is announced before the loser steps out. Requesting the seat now would be
        // granted the next free input device (device 2, announced as "player 3"), not seat 2, and
        // the instance would be invisible to both the game and the seat map — so the incoming
        // waits, and the rotation stays unmarked so the next poll retries.
        let room = rotation(2, "watcher");
        let mut current = None;
        let mut state = state_primed(Some(2));
        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        let port = server.local_addr().unwrap().port();
        reconcile_rotation(&mut state, &room, "watcher", &mut current, port);
        let mut buffer = [0u8; 8];
        assert!(server.recv_from(&mut buffer).is_err());
        assert_eq!(current, None);
        assert_eq!(state.last, Some(2));
        assert!(state.waiting_logged);
    }

    #[test]
    fn the_queue_head_steps_in_after_the_loser_steps_out() {
        let mut current = None;
        let mut state = state_primed(Some(2));
        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let (len, _) = server.recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..len], b"NETPLAY_GAME_WATCH\n");
        });
        // The first poll sees the loser still seated: it waits.
        reconcile_rotation(
            &mut state,
            &rotation(2, "watcher"),
            "watcher",
            &mut current,
            port,
        );
        assert_eq!(current, None);
        // The loser steps out and the room catches up: the retry takes the seat.
        reconcile_rotation(
            &mut state,
            &freed_rotation("watcher"),
            "watcher",
            &mut current,
            port,
        );
        handle.join().unwrap();
        assert_eq!(current, Some(2));
        assert_eq!(state.last, Some(3));
        assert!(!state.waiting_logged);
    }

    #[test]
    fn the_awaiting_coin_names_the_seated_incoming_until_their_coin_lands() {
        let room = seated_rotation("watcher");
        // The game has not seen a coin for seat 2 yet.
        assert_eq!(
            awaiting_coin(&room, &snapshot(0x01, 0x00)),
            Some("watcher".to_string())
        );
        // The coin flipped the seat human: the prompt clears.
        assert_eq!(awaiting_coin(&room, &snapshot(0x01, 0x01)), None);
    }

    #[test]
    fn the_awaiting_coin_is_quiet_until_the_incoming_holds_the_seat() {
        // The handover is still pending (the loser holds the seat), so the UI shows the rotation
        // rather than a coin prompt.
        let room = rotation(2, "watcher");
        assert_eq!(awaiting_coin(&room, &snapshot(0x01, 0x00)), None);
    }

    #[test]
    fn the_awaiting_coin_reads_p1s_byte_when_the_incoming_takes_seat_one() {
        let mut room = room_with(&[(1, "watcher"), (2, "player-one")]);
        room.set_rotation(Rotation {
            id: 3,
            loser_slot: 1,
            incoming: Some("watcher".into()),
        });
        assert_eq!(
            awaiting_coin(&room, &snapshot(0x00, 0x01)),
            Some("watcher".to_string())
        );
        assert_eq!(awaiting_coin(&room, &snapshot(0x01, 0x01)), None);
    }

    #[test]
    fn an_uninvolved_player_does_not_toggle() {
        let room = rotation(2, "watcher");
        let mut current = Some(1);
        let mut state = state_primed(Some(2)); // already primed; rotation 3 is new
                                               // A port with no listener: sending must not happen, or the test's socket stays silent.
        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        reconcile_rotation(&mut state, &room, "player-one", &mut current, port);
        let mut buffer = [0u8; 8];
        assert!(server.recv_from(&mut buffer).is_err());
        assert_eq!(state.last, Some(3));
    }

    #[test]
    fn the_first_room_only_primes_the_rotation() {
        let room = rotation(2, "watcher");
        let mut current = Some(2);
        let mut state = RotationState::default();
        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        reconcile_rotation(&mut state, &room, "player-two", &mut current, port);
        assert_eq!(state.last, Some(3));
        assert!(state.primed);
        assert_eq!(current, Some(2));
    }

    #[test]
    fn a_rotation_survives_the_beacon_round_trip() {
        let lobby = full_room_with_waiting_watcher();
        let announced = lobby.announce_rotation(1).unwrap();
        let fetched = query_live_beacon(&lobby);
        assert_eq!(fetched.rotation, Some(announced));
        assert_eq!(fetched.queue, vec!["watcher"]);
        assert_eq!(
            fetched.seats,
            [
                Some("player-one".to_string()),
                Some("player-two".to_string())
            ]
        );
        lobby.stop();
    }

    #[test]
    fn the_queue_head_steps_in_through_the_beacon() {
        let lobby = full_room_with_waiting_watcher();
        lobby.announce_rotation(1).unwrap();
        // The loser's step-out reaches the beacon before the incoming's next poll.
        free_seat(&lobby, 2);
        let fetched = query_live_beacon(&lobby);

        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let (len, _) = server.recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..len], b"NETPLAY_GAME_WATCH\n");
        });

        let mut current = None;
        let mut state = state_primed(None);
        reconcile_rotation(&mut state, &fetched, "watcher", &mut current, port);
        handle.join().unwrap();
        assert_eq!(current, Some(2));
        assert_eq!(state.last, fetched.rotation.map(|rotation| rotation.id));
        lobby.stop();
    }

    #[test]
    fn a_repeated_rotation_id_is_acted_on_once() {
        let lobby = full_room_with_waiting_watcher();
        lobby.announce_rotation(1).unwrap();
        free_seat(&lobby, 2);
        let fetched = query_live_beacon(&lobby);

        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        let port = server.local_addr().unwrap().port();

        let mut current = None;
        let mut state = state_primed(None);
        reconcile_rotation(&mut state, &fetched, "watcher", &mut current, port);
        reconcile_rotation(&mut state, &fetched, "watcher", &mut current, port);

        let mut buffer = [0u8; 64];
        let (len, _) = server.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"NETPLAY_GAME_WATCH\n");
        assert!(server.recv_from(&mut buffer).is_err());
        lobby.stop();
    }

    #[test]
    fn a_room_without_a_rotation_primes_before_acting() {
        let lobby = full_room_with_waiting_watcher();
        let no_rotation = lobby.room().unwrap();
        assert!(no_rotation.rotation.is_none());

        let server = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        let port = server.local_addr().unwrap().port();

        let mut current = None;
        let mut state = RotationState::default();
        reconcile_rotation(&mut state, &no_rotation, "watcher", &mut current, port);
        assert!(state.primed);
        assert_eq!(state.last, None);
        let mut buffer = [0u8; 8];
        assert!(server.recv_from(&mut buffer).is_err());

        lobby.announce_rotation(1).unwrap();
        free_seat(&lobby, 2);
        let fetched = query_live_beacon(&lobby);
        reconcile_rotation(&mut state, &fetched, "watcher", &mut current, port);
        let mut datagram = [0u8; 64];
        let (len, _) = server.recv_from(&mut datagram).unwrap();
        assert_eq!(&datagram[..len], b"NETPLAY_GAME_WATCH\n");
        assert_eq!(current, Some(2));
        lobby.stop();
    }
}
