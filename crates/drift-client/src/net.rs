//! The networked link to a `drift-server`.
//!
//! [`NetClient`] connects over TCP, spawns a background reader thread that decodes
//! [`ServerMessage`] broadcasts, and keeps the latest [`WorldView`] plus a bounded
//! log of recent events behind a mutex. The UI thread never blocks on the socket:
//! each frame it reads the shared state (a cheap clone of a few dozen agents) and
//! renders it. Commands travel the other way through [`send_command`](NetClient::send_command).
//!
//! The client renders from an *owned* mirror because the server sends a borrowed
//! `Snapshot` serialized to JSON; [`WorldView`] is the owned deserialize target.

use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;

use drift_economy::{Command, SimEvent};
use drift_proto::{read_msg, write_msg, ClientMessage, ServerMessage, WorldView};

/// How many recent events the client keeps for the log panel.
const EVENT_LOG_CAP: usize = 2000;

/// State shared between the reader thread and the UI thread.
struct Shared {
    /// The most recent full world view, or `None` until the first arrives.
    latest: Option<WorldView>,
    /// Recent events (bounded ring), accumulated across broadcasts.
    events: VecDeque<SimEvent>,
    /// Whether the reader thread still believes the connection is live.
    connected: bool,
}

/// A connection to an authoritative server.
pub struct NetClient {
    addr: String,
    shared: Arc<Mutex<Shared>>,
    /// The write half, for sending commands to the server
    /// ([`send_command`](NetClient::send_command)).
    writer: Mutex<TcpStream>,
}

impl NetClient {
    /// Connect to `addr` (e.g. `127.0.0.1:4000`) and start receiving broadcasts.
    pub fn connect(addr: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        let reader = stream.try_clone()?;
        let shared = Arc::new(Mutex::new(Shared {
            latest: None,
            events: VecDeque::new(),
            connected: true,
        }));
        let reader_shared = shared.clone();
        thread::spawn(move || reader_loop(reader, reader_shared));
        Ok(Self {
            addr: addr.to_string(),
            shared,
            writer: Mutex::new(stream),
        })
    }

    /// The server address this client connected to.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Whether the connection is still believed live.
    pub fn connected(&self) -> bool {
        self.shared.lock().unwrap().connected
    }

    /// A clone of the latest world view, or `None` if none has arrived yet.
    pub fn latest_view(&self) -> Option<WorldView> {
        self.shared.lock().unwrap().latest.clone()
    }

    /// A clone of the accumulated event log (oldest first).
    pub fn events(&self) -> Vec<SimEvent> {
        self.shared.lock().unwrap().events.iter().cloned().collect()
    }

    /// Send a command to the server. Fails if the connection is gone.
    pub fn send_command(&self, command: Command) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        write_msg(&mut *w, &ClientMessage::Command(command))
    }
}

/// Decode server broadcasts until the connection closes, updating shared state.
fn reader_loop(mut stream: TcpStream, shared: Arc<Mutex<Shared>>) {
    // Loops until the connection closes or a protocol error occurs (read errors).
    while let Ok(ServerMessage::State { events, snapshot, .. }) =
        read_msg::<_, ServerMessage>(&mut stream)
    {
        let mut s = shared.lock().unwrap();
        for e in events {
            s.events.push_back(e);
        }
        while s.events.len() > EVENT_LOG_CAP {
            s.events.pop_front();
        }
        if let Some(value) = snapshot {
            if let Ok(view) = WorldView::from_snapshot_value(value) {
                s.latest = Some(view);
            }
        }
    }
    shared.lock().unwrap().connected = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use drift_data::{ScenarioDef, TraderSpawn};
    use drift_economy::PlayerId;
    use drift_server::{Server, ServerConfig};
    use drift_sim::{load_registry, Session};

    fn mods_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods")
    }

    /// A quiet sandbox: no NPC traders, no piracy, so the only trader that can
    /// appear is the one the test spawns over the network.
    fn sandbox() -> ScenarioDef {
        ScenarioDef {
            name: "net-test-sandbox".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn { count: 0, ship: "core:cobra_mk3".into(), starting_capital: 1000 },
            piracy: None,
            risk_aversion: 0.0,
            escort: None,
            navy: None,
            contract: None,
            loan: None,
            insurance: None,
            future: None,
        }
    }

    /// Poll a condition for up to ~2s (the reader thread updates asynchronously).
    fn poll_until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn receives_state_and_a_networked_spawn_appears() {
        // Resolve ids from an identical local registry, exactly as a real client
        // would (same mods => same interning as the server's world).
        let reg = load_registry(&mods_path()).unwrap();
        let ship = reg.ship_id("core:cobra_mk3").unwrap();
        let at = reg.system_id("core:lave").unwrap();
        let food = reg.commodity_id("core:food").unwrap();
        let session = Session::new(reg, &sandbox(), 1).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let config = ServerConfig { tick_hz: 200.0, snapshot_every: 1 };
        let handle =
            thread::spawn(move || Server::new(session, config).run(listener, server_shutdown));

        let net = NetClient::connect(&addr.to_string()).unwrap();

        // The reader thread should receive the welcome snapshot (no traders yet).
        assert!(poll_until(|| net.latest_view().is_some()), "should receive a world view");
        assert_eq!(net.latest_view().unwrap().traders.len(), 0, "sandbox starts empty");
        assert!(net.connected());

        // Send a Spawn and watch it appear in a received snapshot.
        net.send_command(Command::Spawn { player: PlayerId(0), ship, at, capital: 1000 })
            .unwrap();
        assert!(
            poll_until(|| net.latest_view().map(|v| v.traders.len()) == Some(1)),
            "the spawned trader should appear in a received snapshot"
        );

        // Buy a commodity Lave trades (food) and watch the cargo arrive — a full
        // player round trip: command out, market applied, new state back.
        let trader_id = net.latest_view().unwrap().traders[0].id;
        net.send_command(Command::Buy {
            player: PlayerId(0),
            trader: trader_id,
            commodity: food,
            qty: 3,
        })
        .unwrap();
        assert!(
            poll_until(|| {
                net.latest_view()
                    .map(|v| v.traders.first().map(|t| t.cargo_units()).unwrap_or(0) >= 3)
                    .unwrap_or(false)
            }),
            "the bought cargo should show up on the player's trader"
        );

        shutdown.store(true, Ordering::Relaxed);
        handle.join().unwrap().unwrap();
    }
}
