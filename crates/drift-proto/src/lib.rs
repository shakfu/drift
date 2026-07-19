//! `drift-proto` — the client/server wire contract.
//!
//! This crate is the shared language between a [`drift-server`] host and any
//! client. It has no I/O and no game logic; it only defines what crosses the
//! socket:
//!
//! - the two message types ([`ClientMessage`], [`ServerMessage`]),
//! - the framing that delimits them on a byte stream ([`read_msg`]/[`write_msg`]),
//! - and [`WorldView`], the *owned* mirror a client deserializes a broadcast
//!   snapshot into (the server sends a borrowed `Snapshot`, which is
//!   serialize-only, so a client needs an owned deserialize target).
//!
//! Framing is length-prefixed JSON: a 4-byte big-endian unsigned length followed
//! by that many bytes of a JSON-encoded message. JSON reuses the serde derives
//! already on [`Command`] and [`SimEvent`] and keeps the protocol language-
//! agnostic; the length prefix makes messages self-delimiting over a stream.
//!
//! [`drift-server`]: https://docs.rs/drift-server

use std::io::{self, Read, Write};

use drift_economy::{
    Command, Contract, EncounterView, Future, Loan, Market, Patrol, PiracyStats, Policy, SimEvent,
    Trader,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A message from a client to the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// The handshake a client must send as its **first** message: the
    /// `content_hash` of the mods it loaded locally (from `drift_mods::Registry`).
    /// The server compares it to its own and refuses a mismatch with a
    /// [`ServerMessage::Reject`], because a client running different content
    /// resolves the same name to a different handle and would silently desync.
    Hello { content_hash: u64 },
    /// A player action to apply at the next tick boundary. The server validates
    /// it; an invalid command is dropped, not fatal.
    Command(Command),
}

/// A message from the server to a client.
///
/// The server is authoritative: clients send intents and receive state. A
/// [`State`](ServerMessage::State) is broadcast every tick with that tick's
/// events; the full `snapshot` rides along only on the ticks the server chooses
/// to send it (on connect, and every `snapshot_every` ticks) to bound bandwidth.
/// The snapshot is carried as a `serde_json::Value` so the server can embed the
/// borrowed `Snapshot` without an owned mirror type; a client turns it into a
/// [`WorldView`] with [`WorldView::from_snapshot_value`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    State {
        tick: u64,
        events: Vec<SimEvent>,
        snapshot: Option<serde_json::Value>,
    },
    /// The server refused the connection during the handshake and is about to
    /// close it. Sent instead of a welcome [`State`](ServerMessage::State) when
    /// the client's [`Hello`](ClientMessage::Hello) content hash does not match
    /// the server's (or the client did not handshake first). `reason` is a
    /// human-readable diagnostic for the client to show the player.
    Reject { reason: String },
}

/// An owned mirror of the mutable world state a client renders from.
///
/// It deserializes from the same JSON the server produces by serializing its
/// borrowed `Snapshot`. Only the fields a client needs are declared; serde
/// ignores the rest (`rng`, `markets`, `progress`, `next_trader_id`), so a
/// snapshot deserializes into a `WorldView` without those types having to be
/// carried client-side (`rng`, `progress`, `next_trader_id` are ignored). Add
/// fields here as clients grow to need them — the wire format already carries them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WorldView {
    /// The tick this view is from.
    pub tick: drift_core::Tick,
    /// Per-system markets (prices and stock), indexed by system id — what a
    /// player client needs to decide buys and sells.
    pub markets: Vec<Market>,
    /// Every trader (NPC and player) and where it is.
    pub traders: Vec<Trader>,
    /// The roaming pirate fleet.
    pub pirates: Vec<Patrol>,
    /// The roaming navy fleet.
    pub navy: Vec<Patrol>,
    /// Cumulative piracy tallies.
    pub piracy: PiracyStats,
    /// The live delivery-contract board. `#[serde(default)]` so a snapshot from a
    /// server without contracts (an empty or absent field) still deserializes.
    #[serde(default)]
    pub contracts: Vec<Contract>,
    /// Outstanding loans against traders' capital.
    #[serde(default)]
    pub loans: Vec<Loan>,
    /// Active insurance policies.
    #[serde(default)]
    pub policies: Vec<Policy>,
    /// Open futures positions.
    #[serde(default)]
    pub futures: Vec<Future>,
    /// Battles currently playing out across ticks (for animating live combat).
    #[serde(default)]
    pub encounters: Vec<EncounterView>,
}

impl WorldView {
    /// Interpret a [`ServerMessage::State`]'s `snapshot` value as a `WorldView`.
    pub fn from_snapshot_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}

/// The largest message body we will read, as a guard against a bogus or hostile
/// length prefix allocating unbounded memory. 64 MiB is far above any legitimate
/// snapshot for this simulation.
const MAX_MSG_BYTES: u32 = 64 * 1024 * 1024;

/// Encode a message as length-prefixed JSON bytes, ready to write to a socket.
/// Broadcasting serializes once and writes the same bytes to every client.
pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(msg)?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Write one framed message and flush.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let bytes = encode(msg)?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Read one framed message, blocking until a full frame arrives. Returns the
/// underlying error on EOF or a short read, so callers treat a disconnect as an
/// error and stop.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MSG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {len} exceeds the {MAX_MSG_BYTES}-byte cap"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drift_core::{ShipId, SystemId};
    use drift_economy::PlayerId;

    #[test]
    fn client_message_round_trips_through_framing() {
        let msg = ClientMessage::Command(Command::Spawn {
            player: PlayerId(0),
            ship: ShipId(1),
            at: SystemId(2),
            capital: 1000,
        });
        let mut buf: Vec<u8> = Vec::new();
        write_msg(&mut buf, &msg).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let back: ClientMessage = read_msg(&mut cursor).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn oversized_length_prefix_is_rejected() {
        // A 4-byte length of 0xFFFFFFFF with no body; must error, not allocate.
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF];
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        let res: io::Result<ClientMessage> = read_msg(&mut cursor);
        assert!(res.is_err());
    }

    #[test]
    fn world_view_deserializes_a_real_snapshot() {
        use std::path::PathBuf;

        // A running world produces a snapshot; the client's owned WorldView must
        // deserialize the exact JSON the server would send.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut session =
            drift_sim::Session::load(&root.join("mods"), &root.join("scenarios/frontier.ron"), Some(3))
                .unwrap();
        session.run(200);

        let world = session.world();
        let value = serde_json::to_value(session.snapshot()).unwrap();
        let view = WorldView::from_snapshot_value(value).unwrap();

        assert_eq!(view.tick, world.tick_count());
        assert_eq!(view.markets.len(), world.markets().len());
        assert_eq!(view.traders.len(), world.traders().len());
        assert_eq!(view.pirates.len(), world.pirates().len());
        assert_eq!(view.navy.len(), world.navy().len());
        assert_eq!(view.piracy, world.piracy_stats());
        assert_eq!(view.contracts.as_slice(), world.contracts());
        assert_eq!(view.loans.as_slice(), world.loans());
        assert_eq!(view.policies.as_slice(), world.policies());
        assert_eq!(view.futures.as_slice(), world.futures());
        assert_eq!(view.encounters, world.encounter_views());
    }
}
