use super::beacon::Beacon;
use super::room::Room;
use super::sets::{plan_rotation, Rotation, Seats, SetScore};
use crate::netplay::NetplayInfo;
use crate::sync::MutexExt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct Lobby {
    inner: Mutex<Option<Live>>,
}

struct Live {
    room: Arc<Mutex<Room>>,
    beacon: Beacon,
    /// The host's own netplay nickname, used to seat it and to keep it out of the waiting queue.
    own_nick: String,
}

impl Lobby {
    pub(crate) fn start(
        &self,
        room: Room,
        addresses: &[IpAddr],
        port: u16,
        own_nick: String,
    ) -> crate::error::Result<Room> {
        let mut inner = self.inner.lock_or_recover();
        if inner.is_some() {
            return Err("a room is already hosting".into());
        }
        let starting = room.clone();
        let shared = Arc::new(Mutex::new(room));
        let beacon = Beacon::start(addresses, port, Arc::clone(&shared))?;
        log::info!(
            "room {} beacon listening on {:?}",
            starting.room_id,
            beacon.local_addresses()
        );
        *inner = Some(Live {
            room: shared,
            beacon,
            own_nick,
        });
        Ok(starting)
    }

    pub(crate) fn stop(&self) {
        if let Some(live) = self.inner.lock_or_recover().take() {
            log::info!(
                "room {} beacon stopped",
                live.room.lock_or_recover().room_id
            );
            live.beacon.stop();
        }
    }

    pub(crate) fn room(&self) -> Option<Room> {
        let inner = self.inner.lock_or_recover();
        inner
            .as_ref()
            .map(|live| live.room.lock_or_recover().clone())
    }

    pub(crate) fn own_nick(&self) -> Option<String> {
        let inner = self.inner.lock_or_recover();
        inner.as_ref().map(|live| live.own_nick.clone())
    }

    /// The bound port of the live room's beacon. Same as the configured port, except when started
    /// on port 0 (tests), where it is the OS-assigned one.
    #[cfg(test)]
    pub(crate) fn beacon_port(&self) -> Option<u16> {
        let inner = self.inner.lock_or_recover();
        inner.as_ref().and_then(|live| live.beacon.local_port())
    }

    /// Mutate the live room under the lock, returning the updated clone. `Room`'s setters only
    /// bump the revision when something actually changed, so repeated no-op updates are cheap.
    pub(crate) fn update(&self, change: impl FnOnce(&mut Room)) -> Option<Room> {
        let inner = self.inner.lock_or_recover();
        let live = inner.as_ref()?;
        let mut room = live.room.lock_or_recover();
        change(&mut room);
        Some(room.clone())
    }

    pub(crate) fn set_score(&self, score: SetScore) -> Option<Room> {
        self.update(|room| room.set_score(score))
    }

    /// Publish the coin the game is waiting for (or clear it), so every machine's UI can prompt
    /// the incoming player.
    pub(crate) fn set_awaiting_coin(&self, nick: Option<String>) -> Option<Room> {
        self.update(|room| room.set_awaiting_coin(nick))
    }

    /// Rebuild the seat map from the host's netplay observation: seated players (plus the host's
    /// own slot) fill the two seats, and everyone else connected is a spectator waiting for one,
    /// in the order they started waiting — a player who steps out re-enters at the back.
    pub(crate) fn observe_netplay(&self, info: &NetplayInfo) -> Option<Room> {
        let own_nick = self.own_nick()?;
        self.update(|room| {
            let mut seats = Seats::default();
            if let Some(slot) = info.self_player {
                seats.set(slot, Some(own_nick.clone()));
            }
            for player in &info.players {
                seats.set(player.player, Some(player.nick.clone()));
            }
            for nick in &info.waiting {
                if nick != &own_nick && seats.slot_of(nick).is_none() {
                    seats.enqueue(nick.clone());
                }
            }
            room.set_seats(&seats);
        })
    }

    /// Publish the set-end rotation for the winner's slot, if a spectator is waiting. Each machine
    /// (host included) then toggles its own instance once from the advertised instruction.
    pub(crate) fn announce_rotation(&self, winner_slot: u8) -> Option<Rotation> {
        let inner = self.inner.lock_or_recover();
        let live = inner.as_ref()?;
        let mut room = live.room.lock_or_recover();
        let rotation = plan_rotation(&room.seats(), winner_slot, room.revision.wrapping_add(1))?;
        room.set_rotation(rotation.clone());
        Some(rotation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netplay::{NetplayInfo, NetplayPlayer};
    use std::net::Ipv4Addr;

    const LOOPBACK: [IpAddr; 1] = [IpAddr::V4(Ipv4Addr::LOCALHOST)];

    /// `waiting` is the connected spectators in the order they started waiting; `connections` is
    /// the same set in connection order unless a test overrides it.
    fn info(self_player: Option<u8>, players: &[(&str, u8)], waiting: &[&str]) -> NetplayInfo {
        let waiting: Vec<String> = waiting.iter().map(|nick| (*nick).to_string()).collect();
        NetplayInfo {
            self_player,
            players: players
                .iter()
                .map(|(nick, player)| NetplayPlayer {
                    nick: (*nick).to_string(),
                    player: *player,
                    ping_ms: None,
                })
                .collect(),
            connections: waiting.clone(),
            waiting,
            ..NetplayInfo::default()
        }
    }

    fn lobby_with_room() -> Lobby {
        let lobby = Lobby::default();
        let room = Room::new("r", "node", "player-one", "sfiii3nr1", 2, Some(1));
        lobby
            .start(room, &LOOPBACK, 0, "player-one".into())
            .unwrap();
        lobby
    }

    #[test]
    fn netplay_observation_seats_the_host_and_players() {
        let lobby = lobby_with_room();
        let room = lobby
            .observe_netplay(&info(Some(1), &[("player-two", 2)], &[]))
            .unwrap();
        assert_eq!(room.seats[0].as_deref(), Some("player-one"));
        assert_eq!(room.seats[1].as_deref(), Some("player-two"));
        assert!(room.queue.is_empty());
        assert_eq!(room.phase, super::super::room::RoomPhase::Playing);
    }

    #[test]
    fn extra_connections_wait_in_the_order_they_started_waiting() {
        let lobby = lobby_with_room();
        let room = lobby
            .observe_netplay(&info(
                Some(1),
                &[("player-two", 2)],
                &["watcher-a", "watcher-b"],
            ))
            .unwrap();
        assert_eq!(room.queue, vec!["watcher-a", "watcher-b"]);
        assert_eq!(room.spectators, 2);
    }

    #[test]
    fn a_rotated_out_loser_queues_behind_the_spectators_already_waiting() {
        // `player-two` connected first and lost the set, so it stepped out of seat 2 after
        // `watcher-a` had already been waiting: the queue head must stay `watcher-a`, otherwise
        // the next rotation would hand the seat straight back to the loser.
        let lobby = lobby_with_room();
        let mut info = info(Some(1), &[], &["watcher-a", "player-two"]);
        info.connections = vec!["player-two".into(), "watcher-a".into()];
        let room = lobby.observe_netplay(&info).unwrap();
        assert_eq!(room.queue, vec!["watcher-a", "player-two"]);
    }

    #[test]
    fn announcing_a_rotation_names_the_loser_slot_and_queue_head() {
        let lobby = lobby_with_room();
        lobby
            .observe_netplay(&info(Some(1), &[("player-two", 2)], &["watcher-a"]))
            .unwrap();
        let rotation = lobby.announce_rotation(1).unwrap();
        assert_eq!(rotation.loser_slot, 2);
        assert_eq!(rotation.incoming.as_deref(), Some("watcher-a"));
        assert_eq!(lobby.room().unwrap().rotation, Some(rotation));
    }

    #[test]
    fn announcing_a_rotation_without_a_queue_does_nothing() {
        let lobby = lobby_with_room();
        lobby
            .observe_netplay(&info(Some(1), &[("player-two", 2)], &[]))
            .unwrap();
        assert!(lobby.announce_rotation(2).is_none());
        assert!(lobby.room().unwrap().rotation.is_none());
    }

    #[test]
    fn a_spectating_host_seats_joiners_one_then_two() {
        let lobby = Lobby::default();
        let room = Room::new("r", "node", "player-one", "sfiii3nr1", 2, None);
        lobby
            .start(room, &LOOPBACK, 0, "player-one".into())
            .unwrap();
        let room = lobby
            .observe_netplay(&info(None, &[("player-two", 1)], &["player-two"]))
            .unwrap();
        assert_eq!(room.seats[0].as_deref(), Some("player-two"));
    }
}
