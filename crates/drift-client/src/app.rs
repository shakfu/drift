//! The eframe application: an observer view of the living galaxy, rendering from
//! either an **in-process** simulation or a **networked** server.
//!
//! Rendering is written once against a read-model ([`ViewData`]) that a [`Source`]
//! materializes each frame. A [`Source::Local`] owns a `drift-sim` [`Session`] and
//! advances it on a **fixed timestep** decoupled from the render frame rate (so
//! pausing and speed never affect determinism — the sim only ever advances by
//! whole ticks). A [`Source::Remote`] instead reads the latest state a
//! [`NetClient`] received from an authoritative server and interpolates agent
//! motion between the discrete ticks it is sent. Either way the renderer is a pure
//! read — the simulation crates know nothing about egui.

use std::collections::HashMap;
use std::sync::Arc;

use drift_core::{CommodityId, Quantity, ShipId, SystemId, Tick};
use drift_economy::{
    Command, Contract, ContractKind, EncounterView, EventCategory, Future, FutureSide, Loan, Market,
    Owner, Patrol, PatrolLocation, PiracyStats, PlayerId, Policy, SimEvent, Trader, TraderLocation,
};
use drift_mods::Registry;
use drift_sim::Session;
use eframe::egui;

use crate::net::NetClient;

/// Cap on sim ticks executed per frame, so a stall cannot spiral (local only).
const MAX_TICKS_PER_FRAME: u32 = 400;

/// The per-frame read-model the renderer consumes. Both sources fill this with
/// owned copies (a few dozen agents), so the renderer never touches a `World`, a
/// `Session`, or a socket.
#[derive(Default)]
pub struct ViewData {
    /// The tick this view is from.
    pub tick: u64,
    /// Fractional tick used to interpolate in-transit agents.
    pub now_f: f64,
    pub traders: Vec<Trader>,
    pub pirates: Vec<Patrol>,
    pub navy: Vec<Patrol>,
    pub piracy: PiracyStats,
    /// Per-system markets (prices/stock), indexed by system id — for the pilot
    /// panel's buy/sell decisions.
    pub markets: Vec<Market>,
    /// Recent events for the log panel (oldest first).
    pub events: Vec<SimEvent>,
    /// The live delivery-contract board, for the contracts panel.
    pub contracts: Vec<Contract>,
    /// Outstanding loans, for the finance panel.
    pub loans: Vec<Loan>,
    /// Active insurance policies.
    pub policies: Vec<Policy>,
    /// Open futures positions.
    pub futures: Vec<Future>,
    /// Battles currently playing out, for on-map markers.
    pub encounters: Vec<EncounterView>,
    /// Connection status line for a remote source; `None` when local.
    pub status: Option<String>,
}

/// Where the rendered state comes from. `Local` is boxed because it owns a whole
/// `Session` (far larger than the remote variant).
enum Source {
    Local(Box<LocalSource>),
    Remote(RemoteSource),
}

impl Source {
    /// Advance (local) or poll (remote), then materialize the current state.
    fn update(&mut self, dt: f64, out: &mut ViewData) {
        match self {
            Source::Local(s) => s.update(dt, out),
            Source::Remote(s) => s.update(dt, out),
        }
    }

    /// Submit a player command. In-process it is queued on the local `Session`
    /// (applied at the next tick); networked it is sent to the server. Either way
    /// the command is validated authoritatively before it takes effect, so the UI
    /// can issue optimistically and let a rejection simply do nothing.
    fn queue_command(&mut self, command: Command) {
        match self {
            Source::Local(s) => s.session.queue_command(command),
            Source::Remote(s) => {
                let _ = s.net.send_command(command);
            }
        }
    }
}

/// An in-process simulation the client both runs and renders.
struct LocalSource {
    session: Session,
    paused: bool,
    /// Simulation speed in ticks per second.
    speed: f64,
    /// Fractional tick accumulator for the fixed-timestep loop.
    accum: f64,
}

impl LocalSource {
    /// Run whole ticks for the elapsed wall time, carrying the fraction across
    /// frames so speed changes never drop or double a tick.
    fn advance(&mut self, dt: f64) {
        if self.paused {
            return;
        }
        self.accum += dt * self.speed;
        let mut n = 0;
        while self.accum >= 1.0 && n < MAX_TICKS_PER_FRAME {
            self.session.world_mut().tick();
            self.accum -= 1.0;
            n += 1;
        }
    }

    /// Advance the sim exactly one tick (the Step button).
    fn step_once(&mut self) {
        self.session.world_mut().tick();
    }

    fn update(&mut self, dt: f64, out: &mut ViewData) {
        self.advance(dt);
        let w = self.session.world();
        out.tick = w.tick_count().get();
        out.now_f = out.tick as f64 + self.accum;
        out.traders = w.traders().to_vec();
        out.pirates = w.pirates().to_vec();
        out.navy = w.navy().to_vec();
        out.piracy = w.piracy_stats();
        out.markets = w.markets().to_vec();
        out.contracts = w.contracts().to_vec();
        out.loans = w.loans().to_vec();
        out.policies = w.policies().to_vec();
        out.futures = w.futures().to_vec();
        out.encounters = w.encounter_views();
        out.events = w.events().cloned().collect();
        out.status = None;
    }
}

/// A view of a remote authoritative server. The server ticks at its own (low)
/// rate; the client interpolates agent motion between the ticks it receives using
/// a running estimate of the inter-tick wall-clock interval.
struct RemoteSource {
    net: NetClient,
    /// The last server tick we have rendered, or `u64::MAX` before the first.
    last_tick: u64,
    /// Wall time elapsed since `last_tick` changed.
    since_tick: f64,
    /// Smoothed estimate of the server's seconds-per-tick.
    period_est: f64,
}

impl RemoteSource {
    fn new(net: NetClient) -> Self {
        Self {
            net,
            last_tick: u64::MAX,
            since_tick: 0.0,
            // Seed with the server's default rate (4 Hz) until we measure it.
            period_est: 0.25,
        }
    }

    fn update(&mut self, dt: f64, out: &mut ViewData) {
        self.since_tick += dt;

        if let Some(view) = self.net.latest_view() {
            let tick = view.tick.get();
            if tick != self.last_tick {
                // A newer tick arrived: `since_tick` is roughly one server period.
                if self.last_tick != u64::MAX && self.since_tick > 0.0 {
                    // Exponential moving average smooths jitter.
                    self.period_est = 0.8 * self.period_est + 0.2 * self.since_tick;
                }
                self.last_tick = tick;
                self.since_tick = 0.0;
            }
            let frac = if self.period_est > 0.0 {
                (self.since_tick / self.period_est).clamp(0.0, 1.0)
            } else {
                0.0
            };
            out.tick = tick;
            out.now_f = tick as f64 + frac;
            out.traders = view.traders;
            out.pirates = view.pirates;
            out.navy = view.navy;
            out.piracy = view.piracy;
            out.markets = view.markets;
            out.contracts = view.contracts;
            out.loans = view.loans;
            out.policies = view.policies;
            out.futures = view.futures;
            out.encounters = view.encounters;
        } else {
            out.tick = 0;
            out.now_f = 0.0;
            out.traders.clear();
            out.pirates.clear();
            out.navy.clear();
            out.markets.clear();
            out.contracts.clear();
            out.loans.clear();
            out.policies.clear();
            out.futures.clear();
            out.encounters.clear();
            out.piracy = PiracyStats::default();
        }

        out.events = self.net.events();
        out.status = Some(if self.net.connected() {
            format!("connected to {}", self.net.addr())
        } else {
            format!("disconnected from {}", self.net.addr())
        });
    }
}

pub struct DriftApp {
    reg: Arc<Registry>,
    source: Source,
    /// Galaxy coordinate bounds (min_x, min_y, max_x, max_y).
    bounds: (f64, f64, f64, f64),
    /// The current frame's read-model, reused across frames.
    view: ViewData,
    /// Which event categories the log panel shows.
    show_combat: bool,
    show_piracy: bool,
    show_navy: bool,
    show_system: bool,
    /// The player this client controls (a trader is "ours" when owned by it).
    player: PlayerId,
    /// Ship the launch button spawns.
    spawn_ship: ShipId,
    /// Currently selected launch system and starting capital.
    spawn_system: SystemId,
    spawn_capital: i64,
    /// Quantity used by the buy/sell controls.
    trade_qty: u32,
    /// System whose market panel is open, if any (click any node to select).
    selected: Option<SystemId>,
    /// Whether the floating contracts panel is shown.
    show_contracts: bool,
    /// Whether the floating finance panel is shown.
    show_finance: bool,
    /// Principal the Borrow control will request.
    borrow_amount: i64,
    /// Commodity and quantity the futures control will trade.
    future_commodity: CommodityId,
    future_qty: u32,
}

impl DriftApp {
    /// An observer of an in-process simulation.
    pub fn local(reg: Arc<Registry>, session: Session) -> Self {
        Self::new(
            reg,
            Source::Local(Box::new(LocalSource {
                session,
                paused: false,
                speed: 10.0,
                accum: 0.0,
            })),
        )
    }

    /// An observer of a remote authoritative server. `reg` is the client's local
    /// copy of the same content the server runs (identical mods => identical
    /// interning, so market/system indices align).
    pub fn remote(reg: Arc<Registry>, net: NetClient) -> Self {
        Self::new(reg, Source::Remote(RemoteSource::new(net)))
    }

    fn new(reg: Arc<Registry>, source: Source) -> Self {
        let (mut minx, mut miny, mut maxx, mut maxy) =
            (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        let mut first_system = SystemId(0);
        for (i, s) in reg.systems().enumerate() {
            let [x, y] = s.position;
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
            if i == 0 {
                first_system = s.id;
            }
        }
        // Default launch ship: the classic Cobra, or the first ship if content
        // differs.
        let spawn_ship = reg.ship_id("core:cobra_mk3").unwrap_or(ShipId(0));
        Self {
            reg,
            source,
            bounds: (minx, miny, maxx, maxy),
            view: ViewData::default(),
            show_combat: true,
            show_piracy: true,
            show_navy: true,
            show_system: true,
            player: PlayerId(0),
            spawn_ship,
            spawn_system: first_system,
            spawn_capital: 1000,
            trade_qty: 5,
            selected: None,
            show_contracts: false,
            show_finance: false,
            borrow_amount: 1000,
            future_commodity: CommodityId(0),
            future_qty: 5,
        }
    }

    /// Whether an event of `category` passes the current log filters.
    fn shows(&self, category: EventCategory) -> bool {
        match category {
            EventCategory::Combat => self.show_combat,
            EventCategory::Piracy => self.show_piracy,
            EventCategory::Navy => self.show_navy,
            EventCategory::System => self.show_system,
        }
    }

    /// Map a galaxy position into the given screen rectangle (y is flipped, since
    /// galaxy y is up and screen y is down).
    fn to_screen(&self, p: [f64; 2], rect: egui::Rect) -> egui::Pos2 {
        let (minx, miny, maxx, maxy) = self.bounds;
        let inner = rect.shrink(50.0);
        let nx = if maxx > minx { (p[0] - minx) / (maxx - minx) } else { 0.5 };
        let ny = if maxy > miny { (p[1] - miny) / (maxy - miny) } else { 0.5 };
        egui::pos2(
            inner.left() + nx as f32 * inner.width(),
            inner.bottom() - ny as f32 * inner.height(),
        )
    }

    /// A dot position fanned around a system node (so co-docked agents don't
    /// overlap). `fan` counts how many agents have already been placed there.
    fn fan_pos(&self, sys: SystemId, fan: &mut HashMap<u32, u32>, rect: egui::Rect) -> egui::Pos2 {
        let i = fan.entry(sys.0).or_insert(0);
        let angle = *i as f32 * 0.9;
        let r = 16.0 + (*i as f32 / 8.0) * 4.0;
        *i += 1;
        let c = self.to_screen(self.reg.system(sys).position, rect);
        egui::pos2(c.x + r * angle.cos(), c.y + r * angle.sin())
    }

    /// Interpolated position of a ship in transit, at fractional tick `now_f`.
    fn transit_pos(
        &self,
        origin: SystemId,
        dest: SystemId,
        departure: Tick,
        arrival: Tick,
        now_f: f64,
        rect: egui::Rect,
    ) -> egui::Pos2 {
        let (d0, d1) = (departure.get() as f64, arrival.get() as f64);
        let prog = if d1 > d0 { ((now_f - d0) / (d1 - d0)).clamp(0.0, 1.0) } else { 1.0 };
        let p0 = self.reg.system(origin).position;
        let p1 = self.reg.system(dest).position;
        let g = [p0[0] + (p1[0] - p0[0]) * prog, p0[1] + (p1[1] - p0[1]) * prog];
        self.to_screen(g, rect)
    }

    fn side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("hud").min_width(240.0).show(ctx, |ui| {
            ui.heading("Drift");
            ui.label(format!("Tick {}", self.view.tick));

            // Source-specific controls / status.
            match &mut self.source {
                Source::Local(s) => {
                    ui.separator();
                    ui.horizontal(|ui| {
                        let label = if s.paused { "Resume" } else { "Pause" };
                        if ui.button(label).clicked() {
                            s.paused = !s.paused;
                        }
                        if ui.button("Step").clicked() {
                            s.step_once();
                        }
                    });
                    ui.add(egui::Slider::new(&mut s.speed, 0.0..=60.0).text("ticks/sec"));
                }
                Source::Remote(_) => {
                    if let Some(status) = &self.view.status {
                        ui.separator();
                        ui.label(status);
                        ui.label("(observing a server; state is authoritative)");
                    }
                }
            }
            ui.separator();

            let p = self.view.piracy;
            ui.label(format!("Traders:  {}", self.view.traders.len()));
            ui.label(format!("Pirates:  {}", self.view.pirates.len()));
            ui.label(format!("Navy:     {}", self.view.navy.len()));
            ui.separator();
            ui.label(format!("Ambushes:          {}", p.ambushes));
            ui.label(format!("Traders lost:      {}", p.traders_lost));
            ui.label(format!(
                "Pirates destroyed: {}",
                p.pirates_destroyed + p.pirates_suppressed
            ));
            ui.label(format!("Bounties paid:     {}", p.bounties_paid));
            ui.separator();

            let open = self.view.contracts.iter().filter(|c| c.is_open()).count();
            let label = format!("Contracts ({open} open) \u{2026}");
            if ui.selectable_label(self.show_contracts, label).clicked() {
                self.show_contracts = !self.show_contracts;
            }
            let fin = format!("Finance ({} loans) \u{2026}", self.view.loans.len());
            if ui.selectable_label(self.show_finance, fin).clicked() {
                self.show_finance = !self.show_finance;
            }
            ui.separator();

            ui.label("Nodes: danger (green safe -> red lawless)");
            ui.colored_label(TRADER, "\u{25CF} traders");
            ui.colored_label(PIRATE, "\u{25CF} pirates");
            ui.colored_label(NAVY, "\u{25CF} navy");
            ui.label("(agents shown at the system they are docked at)");
        });
    }

    /// The pilot panel: control the player's own trader through the command
    /// pipeline. Works in both modes — the command sink queues to the in-process
    /// `Session` (single-player) or sends to the server (networked). The player
    /// learns their trader's server-assigned id simply by finding it in the state,
    /// so no id bookkeeping is needed here.
    fn player_panel(&mut self, ctx: &egui::Context) {
        // Read-only data as owned locals, so the panel closure can mutate UI-state
        // fields without conflicting with borrows of `self`. `reg` is an Arc clone.
        let reg = self.reg.clone();
        let player = self.player;
        let trader = find_player_trader(&self.view.traders, player).cloned();
        let mut pending: Option<Command> = None;

        egui::SidePanel::left("pilot").min_width(250.0).show(ctx, |ui| {
            ui.heading("Pilot");
            match &trader {
                None => {
                    ui.label("You have no ship.");
                    ui.separator();
                    egui::ComboBox::from_label("Launch at")
                        .selected_text(reg.system_name(self.spawn_system).to_string())
                        .show_ui(ui, |ui| {
                            for s in reg.systems() {
                                ui.selectable_value(&mut self.spawn_system, s.id, &s.name);
                            }
                        });
                    ui.add(egui::Slider::new(&mut self.spawn_capital, 100..=5000).text("credits"));
                    let ship_name = reg.ship(self.spawn_ship).name.clone();
                    if ui.button(format!("Launch ({ship_name})")).clicked() {
                        pending = Some(Command::Spawn {
                            player,
                            ship: self.spawn_ship,
                            at: self.spawn_system,
                            capital: self.spawn_capital,
                        });
                    }
                }
                Some(t) => {
                    let id = t.id;
                    ui.label(format!("Ship:    {}", reg.ship(t.ship).name));
                    ui.label(format!("Credits: {}", t.capital));
                    ui.label(format!(
                        "Cargo:   {}/{}",
                        t.cargo_units(),
                        reg.ship(t.ship).cargo_capacity
                    ));
                    ui.separator();

                    match t.location {
                        TraderLocation::Docked(sys) => {
                            ui.strong(format!("Docked at {}", reg.system_name(sys)));
                            ui.add(egui::Slider::new(&mut self.trade_qty, 1..=50).text("qty"));

                            if let Some(market) = self.view.markets.get(sys.0 as usize) {
                                ui.separator();
                                ui.strong("Market  (price / stock / held)");
                                for (c, good) in &market.goods {
                                    let held = t.cargo.get(c).copied().unwrap_or(0);
                                    ui.horizontal(|ui| {
                                        ui.monospace(format!(
                                            "{:<9} {:>4} {:>5} {:>4}",
                                            reg.commodity_name(*c),
                                            good.price,
                                            good.stock,
                                            held
                                        ));
                                        if ui.button("Buy").clicked() {
                                            pending = Some(Command::Buy {
                                                player,
                                                trader: id,
                                                commodity: *c,
                                                qty: self.trade_qty,
                                            });
                                        }
                                        if held > 0 && ui.button("Sell").clicked() {
                                            pending = Some(Command::Sell {
                                                player,
                                                trader: id,
                                                commodity: *c,
                                                qty: self.trade_qty.min(held),
                                            });
                                        }
                                    });
                                }
                            }

                            ui.separator();
                            ui.strong("Jump to");
                            for &dest in &reg.system(sys).connections {
                                if ui.button(reg.system_name(dest)).clicked() {
                                    pending = Some(Command::Jump { player, trader: id, dest });
                                }
                            }
                        }
                        TraderLocation::InTransit { dest, arrival, .. } => {
                            ui.label(format!(
                                "In transit to {} (arrives tick {})",
                                reg.system_name(dest),
                                arrival.get()
                            ));
                        }
                        TraderLocation::Destroyed { respawn } => {
                            ui.label(format!("Destroyed; respawns at tick {}", respawn.get()));
                        }
                    }

                    ui.separator();
                    if ui.button("Retire ship").clicked() {
                        pending = Some(Command::Despawn { player, trader: id });
                    }
                }
            }
        });

        if let Some(cmd) = pending {
            self.source.queue_command(cmd);
        }
    }

    /// A scrolling, colour-coded event log at the bottom of the window, with
    /// per-category filter checkboxes.
    fn log_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(150.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Event log");
                    ui.separator();
                    ui.checkbox(&mut self.show_combat, "combat");
                    ui.checkbox(&mut self.show_piracy, "piracy");
                    ui.checkbox(&mut self.show_navy, "navy");
                    ui.checkbox(&mut self.show_system, "system");
                });

                let events: Vec<&SimEvent> = self
                    .view
                    .events
                    .iter()
                    .filter(|e| self.shows(e.category))
                    .collect();
                ui.label(format!("({} shown)", events.len()));

                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show_rows(ui, row_h, events.len(), |ui, range| {
                        for i in range {
                            let e = events[i];
                            ui.colored_label(
                                category_color(e.category),
                                format!("[{:>5}] {}", e.tick.get(), e.message),
                            );
                        }
                    });
            });
    }

    /// The system whose node contains screen point `p` (within the node pick
    /// radius), or `None`. Factored out of the click handler so it can be tested
    /// without egui input.
    fn system_at(&self, p: egui::Pos2, rect: egui::Rect) -> Option<SystemId> {
        self.reg.systems().find_map(|s| {
            let c = self.to_screen(s.position, rect);
            (c.distance(p) <= NODE_PICK_RADIUS).then_some(s.id)
        })
    }

    fn galaxy_map(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let rect = ui.max_rect();
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(8, 10, 18));

            // Jump edges (drawn once per pair).
            for s in self.reg.systems() {
                let a = self.to_screen(s.position, rect);
                for &c in &s.connections {
                    if c.0 > s.id.0 {
                        let b = self.to_screen(self.reg.system(c).position, rect);
                        painter.line_segment([a, b], egui::Stroke::new(1.0_f32, egui::Color32::from_gray(55)));
                    }
                }
            }

            // System nodes.
            for s in self.reg.systems() {
                let c = self.to_screen(s.position, rect);
                if self.selected == Some(s.id) {
                    painter.circle_stroke(c, 14.0, egui::Stroke::new(2.0_f32, SELECT));
                }
                painter.circle_filled(c, 10.0, danger_color(s.danger));
                painter.circle_stroke(c, 10.0, egui::Stroke::new(1.0_f32, egui::Color32::WHITE));
                painter.text(
                    egui::pos2(c.x, c.y - 14.0),
                    egui::Align2::CENTER_BOTTOM,
                    &s.name,
                    egui::FontId::proportional(13.0),
                    egui::Color32::from_gray(220),
                );
            }

            // Agents: docked ones fanned around their node, in-transit ones
            // interpolated along their jump edge at the current fractional tick.
            let now_f = self.view.now_f;
            let mut fan: HashMap<u32, u32> = HashMap::new();

            for t in &self.view.traders {
                let pos = match t.location {
                    TraderLocation::Docked(sys) => Some(self.fan_pos(sys, &mut fan, rect)),
                    TraderLocation::InTransit { origin, dest, departure, arrival } => {
                        Some(self.transit_pos(origin, dest, departure, arrival, now_f, rect))
                    }
                    TraderLocation::Destroyed { .. } => None,
                };
                if let Some(p) = pos {
                    painter.circle_filled(p, 2.5, TRADER);
                }
            }

            for (fleet, color) in [(&self.view.pirates, PIRATE), (&self.view.navy, NAVY)] {
                for pat in fleet {
                    let p = match pat.location {
                        PatrolLocation::Docked(sys) => self.fan_pos(sys, &mut fan, rect),
                        PatrolLocation::InTransit { origin, dest, departure, arrival } => {
                            self.transit_pos(origin, dest, departure, arrival, now_f, rect)
                        }
                    };
                    painter.circle_filled(p, 2.5, color);
                }
            }

            // Combat flashes: each recent fight pulses a fading, expanding ring at
            // the system where it happened, so battles are visible on the map and
            // not only in the log. Age is measured in (fractional) ticks off the
            // current interpolated time, so flashes decay at sim speed and honour
            // pause. Gated by the same per-category filters as the log.
            for e in &self.view.events {
                let Some(sys) = e.system else { continue };
                if !is_combat(e.category) || !self.shows(e.category) {
                    continue;
                }
                let age = now_f - e.tick.get() as f64;
                if !(0.0..FLASH_LIFETIME).contains(&age) {
                    continue;
                }
                let k = (1.0 - age / FLASH_LIFETIME) as f32; // 1 at birth -> 0 at death
                let c = self.to_screen(self.reg.system(sys).position, rect);
                let radius = 12.0 + (1.0 - k) * 26.0; // expands as it fades
                let base = category_color(e.category);
                let color =
                    egui::Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), (k * 200.0) as u8);
                painter.circle_stroke(c, radius, egui::Stroke::new(2.0_f32, color));
            }

            // Running battles: a persistent pulsing marker at each system where a
            // multi-tick fight is under way, labelled with the live combatant count.
            let pulse = 0.5 + 0.5 * ((now_f * 0.6).sin().abs());
            for enc in &self.view.encounters {
                let c = self.to_screen(self.reg.system(enc.system).position, rect);
                let alive = enc.combatants.iter().filter(|cb| cb.alive).count();
                painter.circle_stroke(c, 16.0 + 4.0 * pulse as f32, egui::Stroke::new(2.5_f32, BATTLE));
                painter.text(
                    egui::pos2(c.x, c.y + 16.0),
                    egui::Align2::CENTER_TOP,
                    format!("\u{2694} {alive}"),
                    egui::FontId::proportional(12.0),
                    BATTLE,
                );
            }

            // Selection: click a node to open its market panel; a click on empty
            // space closes it.
            let response = ui.interact(rect, egui::Id::new("galaxy_click"), egui::Sense::click());
            if response.clicked() {
                if let Some(p) = response.interact_pointer_pos() {
                    let hit = self.system_at(p, rect);
                    self.selected = hit;
                }
            }
        });
    }

    /// A floating panel for the selected system: its danger, market goods (price,
    /// stock, equilibrium, and whether stock is a surplus or shortage), and the
    /// production chains it runs. Read-only and mode-agnostic — the market data is
    /// the same `view.markets` served locally or over the wire.
    fn market_panel(&mut self, ctx: &egui::Context) {
        let Some(sys) = self.selected else { return };
        let reg = self.reg.clone();
        let mut open = true;
        egui::Window::new(format!("Market \u{2014} {}", reg.system_name(sys)))
            .id(egui::Id::new("market_panel"))
            .open(&mut open)
            .default_width(320.0)
            .resizable(true)
            .show(ctx, |ui| {
                let sdef = reg.system(sys);
                ui.label(format!("Danger: {:.2}", sdef.danger));
                ui.separator();

                match self.view.markets.get(sys.0 as usize) {
                    Some(market) if !market.goods.is_empty() => {
                        egui::Grid::new("market_goods").striped(true).show(ui, |ui| {
                            ui.strong("commodity");
                            ui.strong("price");
                            ui.strong("stock");
                            ui.strong("equil");
                            ui.strong("");
                            ui.end_row();
                            for (c, g) in &market.goods {
                                ui.monospace(reg.commodity_name(*c));
                                ui.monospace(format!("{}", g.price));
                                ui.monospace(format!("{}", g.stock));
                                ui.monospace(format!("{}", g.equilibrium));
                                let (label, color) = stock_state(g.stock, g.equilibrium);
                                ui.colored_label(color, label);
                                ui.end_row();
                            }
                        });
                    }
                    _ => {
                        ui.label("No market here.");
                    }
                }

                ui.separator();
                ui.strong("Production");
                if sdef.industries.is_empty() {
                    ui.label("No industries.");
                } else {
                    for &rid in &sdef.industries {
                        let r = reg.recipe(rid);
                        let inputs = fmt_amounts(&reg, &r.inputs);
                        let outputs = fmt_amounts(&reg, &r.outputs);
                        let lhs = if inputs.is_empty() { "\u{2217}".to_string() } else { inputs };
                        ui.monospace(format!("{lhs} \u{2192} {outputs}"));
                    }
                }
            });
        if !open {
            self.selected = None;
        }
    }

    /// A floating panel listing the delivery-contract board: what to deliver,
    /// where, the reward, and ticks left. When the client controls a trader, open
    /// contracts get an **Accept** button, and a contract this trader holds gets a
    /// **Fulfil** button once the trader is docked at the destination with the
    /// cargo. Commands go through the same sink as the pilot panel and are
    /// validated authoritatively, so the buttons can be issued optimistically.
    fn contracts_panel(&mut self, ctx: &egui::Context) {
        if !self.show_contracts {
            return;
        }
        let reg = self.reg.clone();
        let player = self.player;
        let trader = find_player_trader(&self.view.traders, player).cloned();
        let now = self.view.tick;
        let mut open = true;
        let mut pending: Option<Command> = None;

        egui::Window::new("Contracts")
            .id(egui::Id::new("contracts_panel"))
            .open(&mut open)
            .default_width(440.0)
            .resizable(true)
            .show(ctx, |ui| {
                if trader.is_none() {
                    ui.label("Launch a ship to take on contracts.");
                    ui.separator();
                }
                if self.view.contracts.is_empty() {
                    ui.label("No contracts on the board.");
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("contracts_grid").striped(true).show(ui, |ui| {
                            ui.strong("task");
                            ui.strong("to");
                            ui.strong("reward");
                            ui.strong("left");
                            ui.strong("status");
                            ui.strong("");
                            ui.end_row();

                            for c in &self.view.contracts {
                                ui.monospace(contract_task(&reg, c));
                                ui.monospace(reg.system_name(c.destination).to_string());
                                ui.monospace(format!("{}cr", c.reward));
                                ui.monospace(format!("{}t", c.deadline.get().saturating_sub(now)));

                                match c.holder() {
                                    None => {
                                        ui.label("open");
                                        if let Some(t) = &trader {
                                            if ui.button("Accept").clicked() {
                                                pending = Some(Command::AcceptContract {
                                                    player,
                                                    trader: t.id,
                                                    contract: c.id,
                                                });
                                            }
                                        } else {
                                            ui.label("");
                                        }
                                    }
                                    Some((p, tid)) => {
                                        let ours = p == player
                                            && trader.as_ref().map(|t| t.id) == Some(tid);
                                        if ours {
                                            ui.label("yours");
                                            let ready = can_fulfill(trader.as_ref().unwrap(), c);
                                            if ready && ui.button("Fulfil").clicked() {
                                                pending = Some(Command::FulfillContract {
                                                    player,
                                                    trader: tid,
                                                    contract: c.id,
                                                });
                                            } else if !ready {
                                                ui.label("");
                                            }
                                        } else {
                                            ui.label("taken");
                                            ui.label("");
                                        }
                                    }
                                }
                                ui.end_row();
                            }
                        });
                    });
            });

        self.show_contracts = open;
        if let Some(cmd) = pending {
            self.source.queue_command(cmd);
        }
    }

    /// A floating panel for the financial layer: borrow against the player's
    /// trader and repay outstanding loans. Commands go through the same sink as the
    /// pilot panel and are validated authoritatively.
    fn finance_panel(&mut self, ctx: &egui::Context) {
        if !self.show_finance {
            return;
        }
        let reg = self.reg.clone();
        let player = self.player;
        let trader = find_player_trader(&self.view.traders, player).cloned();
        let now = self.view.tick;
        let mut open = true;
        let mut pending: Option<Command> = None;

        egui::Window::new("Finance")
            .id(egui::Id::new("finance_panel"))
            .open(&mut open)
            .default_width(360.0)
            .resizable(true)
            .show(ctx, |ui| {
                let Some(t) = &trader else {
                    ui.label("Launch a ship to use financial services.");
                    return;
                };
                ui.label(format!("Capital: {} cr", t.capital));
                let docked = matches!(t.location, TraderLocation::Docked(_));
                if !docked {
                    ui.label("(dock at a station to borrow)");
                }
                ui.separator();

                ui.strong("Borrow");
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut self.borrow_amount, 100..=20_000).text("cr"));
                    if ui.add_enabled(docked, egui::Button::new("Borrow")).clicked() {
                        pending = Some(Command::TakeLoan {
                            player,
                            trader: t.id,
                            principal: self.borrow_amount,
                        });
                    }
                });
                ui.separator();

                ui.strong("Loans");
                let mine: Vec<&Loan> =
                    self.view.loans.iter().filter(|l| l.borrower == t.id).collect();
                if mine.is_empty() {
                    ui.label("No outstanding loans.");
                } else {
                    egui::Grid::new("loans_grid").striped(true).show(ui, |ui| {
                        ui.strong("id");
                        ui.strong("owed");
                        ui.strong("due in");
                        ui.strong("");
                        ui.end_row();
                        for l in &mine {
                            ui.monospace(format!("#{}", l.id.0));
                            ui.monospace(format!("{}cr", l.outstanding));
                            ui.monospace(format!("{}t", l.due.get().saturating_sub(now)));
                            // Repay as much as the balance and capital allow.
                            let pay = t.capital.min(l.outstanding);
                            if pay > 0 {
                                if ui.button(format!("Repay {pay}")).clicked() {
                                    pending = Some(Command::RepayLoan {
                                        player,
                                        trader: t.id,
                                        loan: l.id,
                                        amount: pay,
                                    });
                                }
                            } else {
                                ui.label("no funds");
                            }
                            ui.end_row();
                        }
                    });
                }
                ui.separator();

                ui.strong("Insurance");
                match self.view.policies.iter().find(|p| p.insured == t.id) {
                    Some(p) => {
                        ui.label(format!(
                            "Covered for {}cr (lapses in {}t)",
                            p.payout,
                            p.expiry.get().saturating_sub(now)
                        ));
                    }
                    None => {
                        if ui.add_enabled(docked, egui::Button::new("Buy insurance")).clicked() {
                            pending = Some(Command::BuyInsurance { player, trader: t.id });
                        }
                    }
                }
                ui.separator();

                ui.strong("Futures");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("future_commodity")
                        .selected_text(reg.commodity_name(self.future_commodity).to_string())
                        .show_ui(ui, |ui| {
                            for (cid, def) in reg.commodities() {
                                ui.selectable_value(&mut self.future_commodity, cid, &def.name);
                            }
                        });
                    ui.add(egui::Slider::new(&mut self.future_qty, 1..=50).text("qty"));
                });
                ui.horizontal(|ui| {
                    for (label, side) in [("Long", FutureSide::Long), ("Short", FutureSide::Short)] {
                        if ui.add_enabled(docked, egui::Button::new(label)).clicked() {
                            pending = Some(Command::OpenFuture {
                                player,
                                trader: t.id,
                                commodity: self.future_commodity,
                                qty: self.future_qty,
                                side,
                            });
                        }
                    }
                });
                let myf: Vec<&Future> =
                    self.view.futures.iter().filter(|f| f.holder == t.id).collect();
                if !myf.is_empty() {
                    egui::Grid::new("futures_grid").striped(true).show(ui, |ui| {
                        ui.strong("side");
                        ui.strong("qty");
                        ui.strong("commodity");
                        ui.strong("strike");
                        ui.strong("matures");
                        ui.end_row();
                        for f in &myf {
                            ui.monospace(format!("{:?}", f.side));
                            ui.monospace(format!("{}", f.quantity));
                            ui.monospace(reg.commodity_name(f.commodity));
                            ui.monospace(format!("{}cr", f.strike));
                            ui.monospace(format!("{}t", f.maturity.get().saturating_sub(now)));
                            ui.end_row();
                        }
                    });
                }
            });

        self.show_finance = open;
        if let Some(cmd) = pending {
            self.source.queue_command(cmd);
        }
    }
}

/// A one-line description of what a contract asks for, for the board's "task"
/// column (the destination is shown separately).
fn contract_task(reg: &Registry, c: &Contract) -> String {
    match c.kind {
        ContractKind::Delivery { commodity, quantity } => {
            format!("{quantity} {}", reg.commodity_name(commodity))
        }
        ContractKind::Courier => "courier parcel".to_string(),
        ContractKind::Bounty { target, progress } => format!("bounty {progress}/{target}"),
    }
}

/// Whether `trader` can fulfil `contract` right now: it holds the contract, is
/// docked at the destination, and meets the kind's condition (cargo aboard for a
/// delivery, quota met for a bounty, arrival alone for a courier). This is the
/// panel's Fulfil-button gate, factored out for testing (the authoritative check
/// still runs in the world when the command is applied).
fn can_fulfill(trader: &Trader, contract: &Contract) -> bool {
    if contract.held_by() != Some(trader.id) {
        return false;
    }
    if !matches!(trader.location, TraderLocation::Docked(s) if s == contract.destination) {
        return false;
    }
    let held = contract
        .cargo()
        .map_or(0, |(commodity, _)| trader.cargo.get(&commodity).copied().unwrap_or(0));
    contract.condition_met(held)
}

/// The trader owned by `player`, if any — the ship this client controls. The
/// player learns its server-assigned id by finding it here after a spawn.
fn find_player_trader(traders: &[Trader], player: PlayerId) -> Option<&Trader> {
    traders.iter().find(|t| t.owner == Owner::Player(player))
}

const TRADER: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);
const PIRATE: egui::Color32 = egui::Color32::from_rgb(230, 70, 70);
const NAVY: egui::Color32 = egui::Color32::from_rgb(80, 220, 220);
/// Highlight ring around the selected system node.
const SELECT: egui::Color32 = egui::Color32::from_rgb(255, 220, 90);
/// Marker for a running (multi-tick) battle at a system.
const BATTLE: egui::Color32 = egui::Color32::from_rgb(255, 140, 40);
/// Screen-pixel radius within which a click selects a system node.
const NODE_PICK_RADIUS: f32 = 12.0;
/// How many (fractional) ticks a combat flash remains visible before it fades
/// out. Measured in ticks so flashes decay at simulation speed and freeze on pause.
const FLASH_LIFETIME: f64 = 4.0;

/// Whether an event category represents a fight (worth flashing on the map).
/// `System` (respawns and other lifecycle notes) is not.
fn is_combat(c: EventCategory) -> bool {
    matches!(
        c,
        EventCategory::Combat | EventCategory::Piracy | EventCategory::Navy
    )
}

/// Classify current stock against its equilibrium anchor, for the market panel:
/// a large surplus reads cheap (green), a shortage dear (orange), else neutral.
fn stock_state(stock: Quantity, equilibrium: Quantity) -> (&'static str, egui::Color32) {
    if equilibrium == 0 {
        return ("", egui::Color32::from_gray(150));
    }
    let ratio = stock as f64 / equilibrium as f64;
    if ratio >= 1.25 {
        ("surplus", egui::Color32::from_rgb(90, 200, 120))
    } else if ratio <= 0.75 {
        ("shortage", egui::Color32::from_rgb(230, 120, 90))
    } else {
        ("normal", egui::Color32::from_gray(150))
    }
}

/// Render a recipe side (inputs or outputs) as "`qty name + qty name`".
fn fmt_amounts(reg: &Registry, amounts: &[(CommodityId, Quantity)]) -> String {
    amounts
        .iter()
        .map(|(c, q)| format!("{} {}", q, reg.commodity_name(*c)))
        .collect::<Vec<_>>()
        .join(" + ")
}

fn category_color(c: EventCategory) -> egui::Color32 {
    match c {
        EventCategory::Combat => egui::Color32::from_rgb(240, 180, 70),
        EventCategory::Piracy => egui::Color32::from_rgb(235, 90, 90),
        EventCategory::Navy => egui::Color32::from_rgb(90, 210, 210),
        EventCategory::System => egui::Color32::from_gray(170),
    }
}

fn danger_color(d: f64) -> egui::Color32 {
    let d = d.clamp(0.0, 1.0) as f32;
    let r = (60.0 + d * (220.0 - 60.0)) as u8;
    let g = (200.0 - d * (200.0 - 40.0)) as u8;
    egui::Color32::from_rgb(r, g, 60)
}

impl eframe::App for DriftApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dt = ctx.input(|i| i.stable_dt) as f64;
        self.source.update(dt, &mut self.view);
        self.side_panel(ctx);
        self.player_panel(ctx);
        self.log_panel(ctx);
        self.galaxy_map(ctx);
        self.market_panel(ctx);
        self.contracts_panel(ctx);
        self.finance_panel(ctx);
        // Keep animating (drives the local sim / picks up remote broadcasts).
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use drift_data::{ScenarioDef, TraderSpawn};
    use drift_sim::load_registry;

    use super::*;

    fn local_source() -> LocalSource {
        let mods = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods");
        let reg = load_registry(&mods).unwrap();
        let scn = ScenarioDef {
            name: "t".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn { count: 0, ship: String::new(), starting_capital: 0 },
            piracy: None,
            risk_aversion: 0.0,
            escort: None,
            navy: None,
            contract: None,
            loan: None,
            insurance: None,
            future: None,
        };
        let session = Session::new(reg, &scn, 1).unwrap();
        LocalSource { session, paused: false, speed: 10.0, accum: 0.0 }
    }

    #[test]
    fn fixed_timestep_advances_by_dt_times_speed() {
        let mut s = local_source();
        s.speed = 10.0;
        s.advance(1.0); // 1s * 10 ticks/s = 10 ticks
        assert_eq!(s.session.world().tick_count().get(), 10);
        // Fractions carry across frames rather than being lost.
        s.advance(0.05); // +0.5 tick -> none yet
        assert_eq!(s.session.world().tick_count().get(), 10);
        s.advance(0.05); // +0.5 -> one whole tick
        assert_eq!(s.session.world().tick_count().get(), 11);
    }

    #[test]
    fn paused_does_not_advance() {
        let mut s = local_source();
        s.paused = true;
        s.advance(10.0);
        assert_eq!(s.session.world().tick_count().get(), 0);
    }

    #[test]
    fn advance_is_capped_per_frame() {
        let mut s = local_source();
        s.speed = 1000.0;
        s.advance(100.0); // ~100k ticks requested; capped to avoid a spiral
        assert_eq!(s.session.world().tick_count().get() as u32, MAX_TICKS_PER_FRAME);
    }

    #[test]
    fn system_at_picks_the_node_under_the_click() {
        let mods = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods");
        let reg = load_registry(&mods).unwrap();
        let scn = ScenarioDef {
            name: "t".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn { count: 0, ship: String::new(), starting_capital: 0 },
            piracy: None,
            risk_aversion: 0.0,
            escort: None,
            navy: None,
            contract: None,
            loan: None,
            insurance: None,
            future: None,
        };
        let session = Session::new(reg.clone(), &scn, 1).unwrap();
        let app = DriftApp::local(reg, session);

        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let sys = app.reg.system_id("core:lave").unwrap();

        // The exact screen center of a node selects that system.
        let center = app.to_screen(app.reg.system(sys).position, rect);
        assert_eq!(app.system_at(center, rect), Some(sys));
        // A point just inside the pick radius still hits.
        let near = egui::pos2(center.x + NODE_PICK_RADIUS - 1.0, center.y);
        assert_eq!(app.system_at(near, rect), Some(sys));
        // A far-off point (well outside any node) selects nothing.
        assert_eq!(app.system_at(egui::pos2(-500.0, -500.0), rect), None);
    }

    #[test]
    fn only_fights_flash_on_the_map() {
        // Combat/piracy/navy events flash; lifecycle (System) events do not.
        assert!(is_combat(EventCategory::Combat));
        assert!(is_combat(EventCategory::Piracy));
        assert!(is_combat(EventCategory::Navy));
        assert!(!is_combat(EventCategory::System));
    }

    #[test]
    fn can_fulfill_requires_holder_location_and_cargo() {
        use drift_economy::{ContractId, ContractState, TraderId};

        let ship = ShipId(0);
        let dest = SystemId(2);
        let food = CommodityId(1);
        let player = PlayerId(0);
        let tid = TraderId(5);

        let contract = Contract {
            id: ContractId(1),
            kind: ContractKind::Delivery { commodity: food, quantity: 10 },
            destination: dest,
            origin: SystemId(0),
            reward: 500,
            deadline: Tick(100),
            state: ContractState::Accepted { player, trader: tid },
        };

        // Holder, docked at the destination, carrying enough: ready.
        let mut ready = Trader::owned(tid, ship, 1000, dest, player);
        ready.cargo.insert(food, 10);
        assert!(can_fulfill(&ready, &contract));

        // One unit short.
        let mut short = Trader::owned(tid, ship, 1000, dest, player);
        short.cargo.insert(food, 9);
        assert!(!can_fulfill(&short, &contract));

        // Laden, but docked at the wrong system.
        let mut elsewhere = Trader::owned(tid, ship, 1000, SystemId(3), player);
        elsewhere.cargo.insert(food, 10);
        assert!(!can_fulfill(&elsewhere, &contract));

        // A different trader is not the holder, even if co-located and laden.
        let mut other = Trader::owned(TraderId(9), ship, 1000, dest, player);
        other.cargo.insert(food, 10);
        assert!(!can_fulfill(&other, &contract));

        // An open (unheld) contract is never fulfillable.
        let open = Contract { state: ContractState::Open, ..contract.clone() };
        assert!(!can_fulfill(&ready, &open));

        // A courier is fulfillable on arrival alone (no cargo needed).
        let courier = Contract {
            kind: ContractKind::Courier,
            ..contract.clone()
        };
        let empty = Trader::owned(tid, ship, 1000, dest, player);
        assert!(can_fulfill(&empty, &courier));

        // A bounty needs its quota met, regardless of cargo.
        let unmet = Contract {
            kind: ContractKind::Bounty { target: 3, progress: 2 },
            ..contract.clone()
        };
        assert!(!can_fulfill(&empty, &unmet));
        let met = Contract {
            kind: ContractKind::Bounty { target: 3, progress: 3 },
            ..contract.clone()
        };
        assert!(can_fulfill(&empty, &met));
    }

    #[test]
    fn stock_state_classifies_surplus_and_shortage() {
        assert_eq!(stock_state(200, 100).0, "surplus");
        assert_eq!(stock_state(50, 100).0, "shortage");
        assert_eq!(stock_state(100, 100).0, "normal");
        // A zero anchor is unclassifiable, not a divide-by-zero.
        assert_eq!(stock_state(10, 0).0, "");
    }

    #[test]
    fn find_player_trader_matches_only_the_owner() {
        use drift_economy::{Trader, TraderId};
        let mods = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods");
        let reg = load_registry(&mods).unwrap();
        let ship = reg.ship_id("core:cobra_mk3").unwrap();
        let at = reg.system_id("core:lave").unwrap();

        let npc = Trader::new(TraderId(1), ship, 100, at);
        let mine = Trader::owned(TraderId(2), ship, 100, at, PlayerId(0));
        let other = Trader::owned(TraderId(3), ship, 100, at, PlayerId(9));
        let traders = vec![npc, mine.clone(), other];

        assert_eq!(find_player_trader(&traders, PlayerId(0)), Some(&mine));
        assert_eq!(find_player_trader(&traders, PlayerId(5)), None);
    }

    #[test]
    fn local_command_sink_drives_a_player_trader() {
        // The pilot panel's command sink, exercised without egui: spawn, then buy,
        // and confirm the world reflects each after a tick.
        let mut source = Source::Local(Box::new(local_source()));
        let reg = {
            let mods = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods");
            load_registry(&mods).unwrap()
        };
        let ship = reg.ship_id("core:cobra_mk3").unwrap();
        let at = reg.system_id("core:lave").unwrap();
        let food = reg.commodity_id("core:food").unwrap();
        let mut view = ViewData::default();

        source.queue_command(Command::Spawn { player: PlayerId(0), ship, at, capital: 1000 });
        source.update(0.2, &mut view); // 0.2s * 10 t/s = 2 ticks -> command applied
        let t = find_player_trader(&view.traders, PlayerId(0)).expect("spawned trader");
        let id = t.id;
        assert_eq!(t.cargo_units(), 0);

        source.queue_command(Command::Buy { player: PlayerId(0), trader: id, commodity: food, qty: 4 });
        source.update(0.2, &mut view);
        let t = find_player_trader(&view.traders, PlayerId(0)).expect("trader still present");
        assert_eq!(t.cargo_units(), 4, "the buy command should have loaded cargo");
    }
}
