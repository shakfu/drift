//! The server loop: a [`Session`] plus a socket.
//!
//! The server is authoritative. It owns the one canonical simulation and is the
//! only thing that advances it. Clients connect over TCP, send [`Command`]s, and
//! receive state; they never touch the world directly. This is the multiplayer
//! model from `docs/dev/multiplayer.md`, and single-player is its N=1 case.
//!
//! Concurrency (std threads only, no async runtime — the simulation is turn-like
//! at a low tick rate, so this is enough and stays trivially testable):
//!
//! - one **accept thread** takes new connections;
//! - one **reader thread per client** decodes incoming [`ClientMessage`]s;
//! - the **sim thread** (this function) owns the `Session`. It selects between
//!   "a client input arrived" and "the next tick is due" via `recv_timeout`,
//!   applies inputs at the tick boundary, and broadcasts state after each tick.
//!
//! All cross-thread traffic flows through one channel of [`Input`] events, so the
//! sim thread mutates the world single-threaded and stays deterministic. Wall-clock
//! is used only to *schedule* ticks; it never enters simulation logic.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use drift_economy::Command;
use drift_proto::{encode, read_msg, write_msg, ClientMessage, ServerMessage};
use drift_sim::Session;

/// Server tuning.
#[derive(Debug, Clone, Copy)]
pub struct ServerConfig {
    /// Simulation ticks per second. The economy is turn-like, so this is low.
    pub tick_hz: f64,
    /// Send a full snapshot every this many ticks (in addition to per-tick
    /// events). `1` sends one every tick; larger values trade freshness for
    /// bandwidth. A snapshot is always sent to a client the moment it connects.
    pub snapshot_every: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tick_hz: 4.0,
            snapshot_every: 5,
        }
    }
}

/// An event delivered to the sim thread from the network side. Funnelling
/// connects, commands, and disconnects through one channel keeps the world
/// mutated by exactly one thread.
enum Input {
    /// A client connected; carries the write half of its socket and the id the
    /// accept thread assigned it.
    Connect(u64, TcpStream),
    /// A client issued a command (already deserialized).
    Command(Command),
    /// A client's reader thread ended (disconnect or protocol error).
    Disconnect(u64),
}

/// A connected client the sim thread broadcasts to.
struct Client {
    id: u64,
    stream: TcpStream,
}

/// The authoritative server. Owns a [`Session`]; [`run`](Server::run) drives it.
pub struct Server {
    session: Session,
    config: ServerConfig,
}

impl Server {
    pub fn new(session: Session, config: ServerConfig) -> Self {
        Self { session, config }
    }

    /// Run the server on `listener` until `shutdown` is set. Blocks the calling
    /// thread (it becomes the sim thread). The accept and per-client reader
    /// threads are spawned internally. Returns when `shutdown` is observed.
    pub fn run(mut self, listener: TcpListener, shutdown: Arc<AtomicBool>) -> std::io::Result<()> {
        let (tx, rx) = mpsc::channel::<Input>();

        // Fingerprint of the content this server runs. Every client must present a
        // matching hash in its handshake, or it is refused (see `client_loop`).
        let content_hash = self.session.registry().content_hash();

        // Accept thread: non-blocking accept + short sleep so it can observe
        // `shutdown` rather than parking forever inside `accept()`.
        listener.set_nonblocking(true)?;
        let accept_shutdown = shutdown.clone();
        let accept_tx = tx.clone();
        let accept =
            thread::spawn(move || accept_loop(listener, accept_tx, accept_shutdown, content_hash));

        let period = Duration::from_secs_f64(1.0 / self.config.tick_hz.max(0.001));
        let snapshot_every = self.config.snapshot_every.max(1);
        let mut clients: Vec<Client> = Vec::new();
        let mut next_tick = Instant::now() + period;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let now = Instant::now();
            if now >= next_tick {
                // Advance one tick (applies commands queued since the last tick
                // in their arrival order) and broadcast the result.
                let events = self.session.step();
                let tick = self.session.world().tick_count().get();
                let snapshot = if tick.is_multiple_of(snapshot_every) {
                    serde_json::to_value(self.session.snapshot()).ok()
                } else {
                    None
                };
                let msg = ServerMessage::State {
                    tick,
                    events,
                    snapshot,
                };
                broadcast(&mut clients, &msg);
                next_tick += period;
                continue;
            }

            match rx.recv_timeout(next_tick - now) {
                Ok(Input::Connect(id, mut stream)) => {
                    // Send the newcomer the current full state immediately, so it
                    // has a baseline before the next delta-free broadcast.
                    let welcome = ServerMessage::State {
                        tick: self.session.world().tick_count().get(),
                        events: Vec::new(),
                        snapshot: serde_json::to_value(self.session.snapshot()).ok(),
                    };
                    if write_msg(&mut stream, &welcome).is_ok() {
                        clients.push(Client { id, stream });
                    }
                }
                Ok(Input::Command(cmd)) => self.session.queue_command(cmd),
                Ok(Input::Disconnect(id)) => clients.retain(|c| c.id != id),
                Err(RecvTimeoutError::Timeout) => {}
                // All senders gone (accept thread and every reader ended). Nothing
                // more can arrive; stop.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // The accept thread polls `shutdown` and exits within its sleep interval.
        let _ = accept.join();
        Ok(())
    }
}

/// Accept connections until `shutdown`. Each connection gets an id and a
/// dedicated thread ([`client_loop`]) that first performs the content handshake
/// and only then admits the client to the sim thread.
fn accept_loop(
    listener: TcpListener,
    tx: Sender<Input>,
    shutdown: Arc<AtomicBool>,
    content_hash: u64,
) {
    let mut next_id: u64 = 0;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let id = next_id;
                next_id += 1;
                // An accepted socket can inherit the listener's non-blocking mode
                // on some platforms; force blocking so `write_all`/`read_exact`
                // park instead of erroring with `WouldBlock`.
                stream.set_nonblocking(false).ok();
                stream.set_nodelay(true).ok();
                let ctx = tx.clone();
                thread::spawn(move || client_loop(id, stream, ctx, content_hash));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                // Transient accept error; back off briefly and retry.
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Serve one client: handshake, then pump its commands until it disconnects.
///
/// The client's **first** message must be a [`ClientMessage::Hello`] carrying the
/// content hash of the mods it loaded. If it matches this server's `content_hash`
/// the client is admitted — its write half is handed to the sim thread via
/// [`Input::Connect`] (which triggers the welcome snapshot) and this thread
/// becomes the client's command reader. On a mismatch (or a first message that is
/// not a `Hello`), the client is refused with a [`ServerMessage::Reject`] and the
/// connection is dropped without ever entering the sim, so a mismatched client
/// can never desync the world.
///
/// The handshake runs on this per-client thread rather than the accept thread, so
/// a slow or silent client cannot stall new connections.
fn client_loop(id: u64, mut stream: TcpStream, tx: Sender<Input>, content_hash: u64) {
    // 1. Handshake. Read exactly one message and require it to be a matching Hello.
    match read_msg::<_, ClientMessage>(&mut stream) {
        Ok(ClientMessage::Hello { content_hash: client_hash }) if client_hash == content_hash => {
            // Accepted; fall through to admit the client.
        }
        Ok(ClientMessage::Hello { content_hash: client_hash }) => {
            let reason = format!(
                "content mismatch: client {client_hash:016x} does not match server {content_hash:016x}; \
                 load the same mods as the server"
            );
            let _ = write_msg(&mut stream, &ServerMessage::Reject { reason });
            return;
        }
        Ok(ClientMessage::Command(_)) => {
            let reason = "protocol error: expected a Hello handshake as the first message".to_string();
            let _ = write_msg(&mut stream, &ServerMessage::Reject { reason });
            return;
        }
        // Client vanished (or sent garbage) before completing the handshake.
        Err(_) => return,
    }

    // 2. Admit: hand the sim thread a write half so it can broadcast to this
    //    client. `try_clone` failing means we drop this client rather than admit a
    //    half-open one.
    let write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    if tx.send(Input::Connect(id, write_half)).is_err() {
        return; // sim thread gone
    }

    // 3. Pump commands until the stream closes (read errors) or the sim thread is
    //    gone. A blocked reader outlives `shutdown`, but that only holds a socket,
    //    and the process exit reclaims it.
    while let Ok(ClientMessage::Command(cmd)) = read_msg::<_, ClientMessage>(&mut stream) {
        if tx.send(Input::Command(cmd)).is_err() {
            break; // sim thread gone
        }
    }
    let _ = tx.send(Input::Disconnect(id));
}

/// Serialize `msg` once and write it to every client, dropping any client whose
/// write fails (a disconnect the sim thread notices before its reader does).
fn broadcast(clients: &mut Vec<Client>, msg: &ServerMessage) {
    let bytes = match encode(msg) {
        Ok(b) => b,
        Err(_) => return,
    };
    clients.retain_mut(|c| c.stream.write_all(&bytes).and_then(|_| c.stream.flush()).is_ok());
}
