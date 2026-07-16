//! End-to-end server test: a client connects, spawns a trader with a command, and
//! sees it appear in a broadcast snapshot. Exercises the whole loop (accept,
//! reader, sim tick, command application, broadcast framing) headlessly over a
//! real loopback TCP socket.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use drift_data::{ScenarioDef, TraderSpawn};
use drift_economy::{Command, PlayerId};
use drift_proto::{read_msg, write_msg, ClientMessage, ServerMessage};
use drift_server::{Server, ServerConfig};
use drift_sim::{load_registry, Session};

fn mods_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods")
}

/// A quiet sandbox: no NPC traders, no piracy, so the only trader that can appear
/// is the one the test spawns.
fn sandbox_scenario() -> ScenarioDef {
    ScenarioDef {
        name: "server-test-sandbox".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn {
            count: 0,
            ship: "core:cobra_mk3".into(),
            starting_capital: 1000,
        },
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

fn trader_count(msg: &ServerMessage) -> Option<usize> {
    let ServerMessage::State { snapshot, .. } = msg;
    let snap = snapshot.as_ref()?;
    Some(snap.get("traders")?.as_array()?.len())
}

#[test]
fn client_spawns_a_trader_and_sees_it_in_a_broadcast() {
    // Resolve content ids the same way the server will (identical mods => identical
    // interning), so the Spawn command's handles are valid in the server's world.
    let reg = load_registry(&mods_path()).unwrap();
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let at = reg.system_id("core:lave").unwrap();

    let session = Session::new(reg, &sandbox_scenario(), 1).unwrap();

    // Bind on an ephemeral port so the test never collides with a real server.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = shutdown.clone();
    // Fast ticks + a snapshot every tick keep the test quick and every broadcast
    // inspectable.
    let config = ServerConfig {
        tick_hz: 200.0,
        snapshot_every: 1,
    };
    let handle = thread::spawn(move || {
        Server::new(session, config).run(listener, server_shutdown)
    });

    let mut client = TcpStream::connect(addr).unwrap();

    // First broadcast (the welcome) carries a snapshot with no traders yet.
    let first: ServerMessage = read_msg(&mut client).unwrap();
    assert_eq!(trader_count(&first), Some(0), "sandbox starts with no traders");

    // Issue a Spawn and wait for a snapshot that reflects it.
    let cmd = Command::Spawn {
        player: PlayerId(0),
        ship,
        at,
        capital: 1000,
    };
    write_msg(&mut client, &ClientMessage::Command(cmd)).unwrap();

    let mut found = false;
    for _ in 0..1000 {
        let msg: ServerMessage = read_msg(&mut client).unwrap();
        if trader_count(&msg) == Some(1) {
            found = true;
            break;
        }
    }
    assert!(found, "the spawned player trader should appear in a broadcast snapshot");

    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
}
