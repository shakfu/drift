# TODO

Outstanding work for Drift. For rationale and sequencing see
[docs/dev/roadmap.md](docs/dev/roadmap.md); for the multiplayer plan see
[docs/dev/multiplayer.md](docs/dev/multiplayer.md). For what is already built, see
[CHANGELOG.md](CHANGELOG.md).

## Near-term (highest leverage)

- [ ] Verify `drift-client` renders on a real display — it is currently
      compile-checked and unit-tested only (no GUI in the dev sandbox).
- [x] Graphical client: per-node **market panels** (click *any* system to show its
      prices, stock vs. equilibrium, and production chains). Click a node to open a
      floating panel; click empty space to close. Reads the same `view.markets`
      served locally or over the wire, so it works in both modes.
- [x] Graphical client: on-map **combat flashes** / ambush markers, so fights are
      visible where they happen (not only in the log). `SimEvent` now carries an
      optional `system`; the client pulses a fading, expanding ring at that node
      for each recent Combat/Piracy/Navy event (decaying in ticks, so it honours
      pause/speed), gated by the same per-category filters as the log.

## Graphical client polish

- [ ] Pan/zoom the galaxy map.
- [x] Ship visuals as data: `ShipDef` carries an optional `visual` block
      (`HullShape` + length/width/height + RGB color), and the 3-D client builds a
      mesh + tinted material per ship type from the registry, rendering each agent
      as its own ship. Engine-agnostic data; excluded from `content_hash` (cosmetic).
      A mod adds a ship's look with no client change.
- [x] A graphical **player** client (input + HUD) over the command pipeline — the
      egui `drift-client` Pilot panel (launch/buy/sell/jump/retire) and the 3-D
      `drift-flight` client (full cockpit input + instrument HUD) both drive it.

## 3-D flight/combat (Bevy) — see [docs/dev/flight-combat.md](docs/dev/flight-combat.md)

- [~] M1 — 3-D combat spectator (`drift-flight`). Renders the galaxy in 3-D and
      animates running battles from `EncounterView`; a pure read of the sim (no
      piloting, determinism firewall intact). The engine-agnostic `scene` model (sim
      -> 3-D geometry) is the tested core; the Bevy app is behind a `gui` feature so
      the default workspace build stays fast and Bevy-free.
      - [ ] Animate agents (traders/pirates/navy) along jump edges, not just battles.
      - [ ] Orbit/pan/zoom camera; HUD; ship visuals as data.
- [~] M2 — single-player in-system real-time flight, wired to the sim. Arcade
      flight (tested `flight` model) with a chase camera; the player is a real
      trader spawned through the command pipeline; the current system is populated
      with the agents the sim reports docked there; jumping (`1-9`) and cockpit
      trading/docking (fly to the station, `Space` to dock, market panel issuing
      `Buy`/`Sell`) all go through the pipeline. Determinism firewall intact
      (free-flight position never enters the sim).
      - [x] Polish: HDR camera + bloom, engine glow, per-system star/planet
            flavour, spinning bodies, a jump flash.
      - [~] Cinematic jump transition; data-driven ship visuals (visuals **done** —
            `ShipDef.visual`; cinematic jump still open).
- [~] M3 — single-player real-time combat. Pirates in the current system spawn as
      live hostiles that steer toward the player and fire; the player fires bolts
      (`F`); a tested `combat::Health` model (shield-then-hull) resolves hits;
      destroyed hostiles are removed and the player respawns on death. Client-side
      and authoritative for the player's fight.
      - [x] Report outcomes to the sim: `DestroyedPirate` (removes the pirate, pays
            the bounty, credits bounty contracts) and `TraderDestroyed` (destroys
            the trader, pays insurance) commands, applied at the tick boundary —
            the flight layer stays authoritative for the player's fight but the sim
            records the consequences. Headless tests cover both.
      - [x] Explosions: a growing emissive flash plus a deterministic burst of
            fragments when any ship dies (player or hostile).
      - [x] Weapon variety: three selectable player weapons (fast Pulse, heavy
            Cannon, three-bolt Scatter), cycled with `Tab`, each with its own
            damage/fire-rate/bolt visuals; shown on the HUD.
      - [x] Elite-style targeting & flight feel. New tested, engine-agnostic
            `targeting` module: projectile **lead/intercept** solver, **radar
            contact** projection, and target-cycling helpers (19 unit tests). Wired
            into the Bevy app: `T` locks/cycles the nearest hostile with a reticle
            and a **lead pip** (fly the nose onto the pip to hit a moving target),
            a bottom-left **contact scanner** (bearing/range/elevation blips),
            **set-and-hold throttle** (`W`/`S`/`X`) instead of momentary thrust,
            a target data panel (range/closing/hull/shield + on-target cue), and
            hostiles that **lead their shots** and orbit rather than firing at
            where you were. **Gimballed vs fixed weapons**: the Pulse auto-tracks a
            locked target within a cone (`targeting::gimbal_aim`), the Cannon and
            Scatter fire straight. Math is verified headlessly; the Bevy rendering
            is compile-checked (needs a display for visual tuning).
      - [~] Escorts/navy fight alongside; ship-visual variety.
            - [x] Navy fights alongside: navy patrols the sim reports in the
                  current system spawn as live **allies** that hunt the nearest
                  hostile, lead their shots (shared `firing_solution`), and orbit —
                  the mirror of the hostile AI, firing friendly bolts. Hostiles now
                  target the nearest of the player *or* an ally, so it is a real
                  skirmish; ally losses are local-only (the sim owns navy
                  attrition, firewall intact). Allies show on the HUD (`NAVY n`) and
                  scanner (teal). Wiring compile/clippy-checked; the shared combat
                  math is unit-tested.
            - [ ] Player-hired escorts as entities; ship-visual variety.
      - [x] Oolite-aligned controls, HUD, and 3-D objects (from the OoliteProject
            source). Controls: roll ←/→, pitch ↑/↓, yaw `,`/`.`, throttle W/S, fire
            A, target T. Objects (procedural, no external assets): a rotating
            Coriolis cuboctahedron station with a docking slot; a **roster of the
            main Oolite ships** as procedural faceted hulls (Cobra Mk III / Viper /
            Sidewinder / Krait / Mamba / Adder / Python / Boa), assigned by role
            (player Cobra, navy Vipers, pirate fighters, trader freighters) via
            `faceted_mesh`/`dart_hull`/`freighter_hull`. HUD: an
            instrument cluster (speed/throttle/hull/shield gauges), a locked-target
            panel with the target's hull/shield bars, centre-zero roll/pitch
            indicators, a compass blip pointing to the station, and a centre
            gunsight; text reduced to nav/jump/market. `combat::Health` gained
            `max_hull`/`hull_frac` (unit-tested). Bevy wiring compile/clippy-checked.
      - [x] Missiles & ECM (Oolite combat layer): lock + fire a homing missile
            (`M`) from limited stores (refilled on docking); tested guidance
            (`targeting::home_missile` — hits stationary/crossing targets, constant
            speed, sluggish = dodgeable); hostiles fire missiles back on a stagger;
            `E` ECM detonates all incoming. HUD shows stores + ECM readiness.
      - [x] Fuel-limited hyperspace + refuel, torus drive / fuel injectors, and
            legal status / bounty. Rules in a tested `nav` module (jump cost by
            distance, affordability, Clean/Offender/Fugitive thresholds). Jumps
            spend fuel and are refused if the tank is short; docking refuels; `J`
            engages a fast mass-locked cruise that burns fuel; attacking the navy
            raises a bounty and a wanted pilot is hunted by the police. FUEL gauge +
            legal-status readout on the HUD.
      - [x] Cargo canisters + scooping: destroyed pirates spill drifting canisters
            (deterministic loot); flying over them scoops the cargo into the hold
            via a new authoritative `Command::ScoopCargo` (free, hold-capped,
            unit-tested), closing the kill -> scoop -> sell loop. HUD shows a
            `CARGO used/cap` readout.
      - [x] Weapon subsystems + a beam laser: tested laser-temperature/overheat
            model (`weapons::WeaponHeat` — fire builds heat, overheats and cuts out
            until cool), a `TEMP` HUD gauge, per-weapon heat, and a continuous beam
            laser (hitscan in a forward cone, rendered as a bar).
      - [x] Improved ship rendering: metallic hulls + tail engine glows on combat
            ships.
      - [ ] More Oolite features (candidates): station equipment/repair,
            witchspace/misjump, fuel scooping from stars, shootable traders (so
            attacking them is also a crime), richer hull meshes / subentities.
      - [ ] Further combat-feel + HUD/visual tuning from playtesting on a display
            (gauge/compass sizing and placement).
- [ ] M4 — real-time multiplayer flight (prediction/rollback; much later, separate).

## Gameplay depth

- [x] Missions and contracts (cargo runs, bounty contracts, courier jobs) on top of
      the existing bounty/economy plumbing. One board with a `ContractKind`
      (Delivery / Courier / Bounty), `AcceptContract`/`FulfillContract` commands,
      per-kind generation + completion in a `contract_phase`, bounty progress
      credited from ambush wins, stable `ContractId` handles, `Snapshot`/`WorldView`
      wiring, a `drift-client` contracts panel, `drift play` CLI support, and full
      test coverage. Enabled in the `equilibrium` scenario.
- [x] Financial instruments (futures, loans, insurance) — the "sophisticated
      trading" differentiator. All three live in `drift-economy::finance`, settle in
      a `finance_phase`, ride on `Snapshot`/`WorldView`, and have a `drift-client`
      Finance panel plus `drift play` commands. Enabled in the `equilibrium`
      scenario.
      - [x] Loans: `TakeLoan`/`RepayLoan`; interest compounds each period, overdue
            loans are called (balance seized).
      - [x] Insurance: `BuyInsurance`; a premium buys a payout if pirates destroy
            the covered trader (rides on the piracy system).
      - [x] Futures: `OpenFuture` (long/short) at the spot price, cash-settled
            against the galaxy reference price at maturity.
- [x] Escort fees / navy funding as real economic costs. Escorts charge a per-jump
      fee to the trader they protect; the navy costs upkeep per ship drawn from a
      public treasury funded by per-tick income, so an underfunded navy runs a
      deficit and shrinks under attrition (reinforcement stalls). Tracked in
      `PiracyStats` (`escort_fees_paid`, `navy_upkeep`) and `World::treasury()`.
- [x] Multi-tick running battles: encounters now play out over several economy
      ticks. `drift-combat`'s `Encounter` is serializable and steppable
      (`advance`); the world holds `ActiveEncounter`s (each with its own RNG),
      opened by the piracy/navy phases and advanced a few steps per tick in a
      `combat_phase`. Participants are addressed by stable `PatrolId`; engaged
      patrols/traders are frozen (no roam/arrival/re-ambush) until the fight
      completes, when the existing casualty/bounty/insurance/contract hooks fire.

## Multiplayer

- [x] Server-authoritative networking transport (`drift-server`: TCP +
      length-prefixed JSON over the command pipeline; std threads, no async).
- [x] Networked **client** (`drift-client --connect`): an owned `WorldView` mirror
      that applies server broadcasts and renders (shared wire contract in
      `drift-proto`).
- [x] Graphical **player** client: a Pilot panel drives launch / buy / sell / jump
      / retire from the UI, through one command sink (local `Session` or server),
      round-trip tested in both modes.
- [x] Content-version handshake: the client sends its linked-content fingerprint
      (`Registry::content_hash`, a dependency-free FNV-1a over the fully-linked
      data in interned order) as a `ClientMessage::Hello` on connect; the server
      refuses a mismatch with a `ServerMessage::Reject` before admitting the
      client to the sim, so mismatched mods fail loudly at connect instead of
      silently desyncing. Headless tests cover both the fingerprint (stable,
      change-sensitive, order-sensitive) and the handshake (match connects,
      mismatch is refused, server survives a refusal).
- [ ] Snapshot delta encoding + interest management (needed only at scale).
- [ ] Client prediction / rollback (needed only for a real-time flight layer).
- [ ] Optional hardening: generational `TraderId` (the current monotonic,
      never-reused id is already ABA-safe; revisit only if it becomes a bottleneck).

## Modding / scripting

- [~] Mod-scripting runtime plugging into the `NamedRegistry` seam. Engine chosen:
      **Rhai** (pure Rust, sandboxed by construction, operation-limited) over
      Lua/WASM — see `drift-script` for the rationale; `wasmi` is the
      determinism-hardened swap-in behind the same seam if lockstep-across-clients
      is ever needed.
      - [x] `drift-script` crate: `ScriptedPricing` (a Rhai-authored market pricing
            strategy), sandboxed + fuel-limited, with tests proving determinism,
            runaway-termination, error containment, and no ambient access.
      - [x] Wire into the pricing seam: `PricingStrategy::Scripted(u32)` +
            `PricingSet` (name registry bundled with the compiled script table);
            `register_script` adds a named scripted strategy, the world keeps the
            table and dispatches it each repricing tick. A system can now name a
            scripted strategy exactly like a built-in; an end-to-end test drives a
            market's prices from a Rhai script.
      - [x] Load `.rhai` scripts declared in a mod manifest (schema + loader), so a
            mod adds behavior from disk. A `[[scripts]]` manifest table (`name`,
            `path`, `kind`) declares a named strategy; the loader reads the `.rhai`
            from disk, enforces unique names, rejects a name that shadows a
            built-in, and folds the declared names into the pricing set it
            validates systems against. `Registry::scripts()` carries the source;
            `drift_economy::pricing_for(&registry)` compiles them into the strategy
            set (built-ins + scripts) at `Session` build (failing the build on a
            compile error). A system now selects a disk-authored scripted strategy
            by name with no programmatic registration; end-to-end and loader tests
            cover the happy path and every failure mode.
      - [ ] More hooks (trader AI, event rules) on the same seam. The `ScriptKind`
            enum and the `[[scripts]]` schema are the extension point.

## Content and balance

- [ ] Larger galaxy: more commodities, ships, and systems.
- [~] Deepen the economy further (the core differentiator).
      - [x] Endogenous manufacturing capacity: transformer industries carry a slow
            capital stock that invests/disinvests with their processing margin and
            scales throughput on top of the price elasticity. Capital is sticky
            (eases at 0.02/tick vs prices at 0.2), giving supply a lagged,
            path-dependent response that settles into a new interior equilibrium
            (manufacturing capacity ~2x, intermediate goods bid up). Lives in
            `production.rs` (`capacity_target`/`eased_capacity`/`recipe_margin`/
            `has_capacity`), rides on `Snapshot`, and is covered by unit tests plus
            an end-to-end "invests and settles" convergence test.
      - [ ] Further mechanisms (trade frictions/tariffs, consumer income dynamics).

## Observability and tooling

- [ ] Richer event set (large NPC trades, reinforcements) and optional event-to-file.
- [x] CI running `make test` and `make lint` (GitHub Actions, `.github/workflows/ci.yml`;
      installs the eframe/egui system libs so the whole workspace builds).
