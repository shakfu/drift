//! Linking: string-id content -> handle-based, cross-checked runtime data.
//!
//! This is where the "link" half of load-and-link happens. Every id is interned
//! to a handle, then every reference (recipe -> commodities, system -> recipes /
//! systems / commodities, system -> pricing strategy) is resolved. A reference
//! that does not resolve aborts the whole load: the resulting [`Registry`] is by
//! construction fully connected, so the simulation never has to handle a dangling
//! id.

use std::collections::HashSet;

use drift_core::{CommodityId, Interner, Quantity, RecipeId, ShipId, SystemId};
use drift_data::{CommodityDef, ShipDef};
use tracing::warn;

use crate::error::LoadError;
use crate::loader::{LoadedScript, MergedContent};
use crate::manifest::ScriptKind;

/// A recipe with all commodity references resolved to handles.
#[derive(Debug, Clone)]
pub struct ResolvedRecipe {
    pub id: RecipeId,
    pub inputs: Vec<(CommodityId, Quantity)>,
    pub outputs: Vec<(CommodityId, Quantity)>,
    pub rate: u32,
    pub elasticity: f64,
}

/// A system with all references resolved to handles.
#[derive(Debug, Clone)]
pub struct ResolvedSystem {
    pub id: SystemId,
    pub name: String,
    pub position: [f64; 2],
    pub industries: Vec<RecipeId>,
    pub connections: Vec<SystemId>,
    pub initial_stock: Vec<(CommodityId, Quantity)>,
    /// Validated pricing strategy name (guaranteed registered by the caller).
    pub pricing: String,
    /// Route lawlessness in `[0, 1]`; scales pirate ambush chance.
    pub danger: f64,
}

/// Immutable, fully-linked game data. Vectors are indexed by the corresponding
/// handle's `.index()`; the interners provide id<->handle and handle->name.
#[derive(Debug)]
pub struct Registry {
    commodity_ids: Interner,
    commodities: Vec<CommodityDef>,
    recipe_ids: Interner,
    recipes: Vec<ResolvedRecipe>,
    system_ids: Interner,
    systems: Vec<ResolvedSystem>,
    ship_ids: Interner,
    ships: Vec<ShipDef>,
    scripts: Vec<LoadedScript>,
}

impl Registry {
    // --- commodities ---
    pub fn commodity_count(&self) -> usize {
        self.commodities.len()
    }
    pub fn commodity(&self, id: CommodityId) -> &CommodityDef {
        &self.commodities[id.index()]
    }
    pub fn commodity_id(&self, name: &str) -> Option<CommodityId> {
        self.commodity_ids.get(name).map(CommodityId)
    }
    pub fn commodity_name(&self, id: CommodityId) -> &str {
        self.commodity_ids.name(id.0).unwrap_or("?")
    }
    pub fn commodities(&self) -> impl Iterator<Item = (CommodityId, &CommodityDef)> {
        self.commodities
            .iter()
            .enumerate()
            .map(|(i, c)| (CommodityId(i as u32), c))
    }

    // --- recipes ---
    pub fn recipe(&self, id: RecipeId) -> &ResolvedRecipe {
        &self.recipes[id.index()]
    }
    pub fn recipe_id(&self, name: &str) -> Option<RecipeId> {
        self.recipe_ids.get(name).map(RecipeId)
    }
    pub fn recipe_name(&self, id: RecipeId) -> &str {
        self.recipe_ids.name(id.0).unwrap_or("?")
    }
    pub fn recipe_count(&self) -> usize {
        self.recipes.len()
    }

    // --- systems ---
    pub fn system_count(&self) -> usize {
        self.systems.len()
    }
    pub fn system(&self, id: SystemId) -> &ResolvedSystem {
        &self.systems[id.index()]
    }
    pub fn system_id(&self, name: &str) -> Option<SystemId> {
        self.system_ids.get(name).map(SystemId)
    }
    pub fn system_name(&self, id: SystemId) -> &str {
        self.system_ids.name(id.0).unwrap_or("?")
    }
    pub fn systems(&self) -> impl Iterator<Item = &ResolvedSystem> {
        self.systems.iter()
    }

    // --- ships ---
    pub fn ship_id(&self, name: &str) -> Option<ShipId> {
        self.ship_ids.get(name).map(ShipId)
    }
    pub fn ship(&self, id: ShipId) -> &ShipDef {
        &self.ships[id.index()]
    }
    pub fn ship_count(&self) -> usize {
        self.ships.len()
    }

    // --- scripts ---
    /// The mod-declared behavior scripts (source + kind + registered name), in
    /// load order. The host compiles these into the matching engine seam (e.g.
    /// pricing) before building a world; the loader has already validated their
    /// names and guaranteed uniqueness.
    pub fn scripts(&self) -> &[LoadedScript] {
        &self.scripts
    }

    /// A stable content fingerprint of the fully-linked registry.
    ///
    /// Two registries produce the same hash if and only if they linked to
    /// byte-identical content in the same interned order. That is exactly the
    /// condition under which a client's id interning matches the server's, so the
    /// networked client/server handshake compares this value to reject a mod
    /// mismatch before it can silently desync a session (a client with different
    /// content resolves the same name to a different handle, or renders a market
    /// the server never had).
    ///
    /// This is FNV-1a over a canonical field-by-field encoding of the *linked*
    /// data, not the RON source. Differences the loader already normalises away
    /// (file layout, comments, declaration order across files) therefore do not
    /// change the hash, while any difference in the resulting game data does.
    /// FNV-1a is dependency-free and deterministic across platforms and builds, so
    /// two separately-built binaries agree on the hash for identical content.
    pub fn content_hash(&self) -> u64 {
        let mut h = Fnv::new();

        // Commodities, in interned (handle) order.
        h.usize(self.commodities.len());
        for (i, c) in self.commodities.iter().enumerate() {
            h.str(self.commodity_ids.name(i as u32).unwrap_or(""));
            h.str(&c.id);
            h.str(&c.name);
            h.i64(c.base_price);
            h.u32(c.unit_mass);
            h.f64(c.elasticity);
            h.str(&c.category);
        }

        // Recipes. Commodity references are hashed as their resolved handle index,
        // so a recipe that points at a different commodity changes the hash.
        h.usize(self.recipes.len());
        for (i, r) in self.recipes.iter().enumerate() {
            h.str(self.recipe_ids.name(i as u32).unwrap_or(""));
            h.usize(r.inputs.len());
            for (c, q) in &r.inputs {
                h.u32(c.0);
                h.u32(*q);
            }
            h.usize(r.outputs.len());
            for (c, q) in &r.outputs {
                h.u32(c.0);
                h.u32(*q);
            }
            h.u32(r.rate);
            h.f64(r.elasticity);
        }

        // Systems. Recipe and system references are hashed as resolved handles.
        h.usize(self.systems.len());
        for (i, s) in self.systems.iter().enumerate() {
            h.str(self.system_ids.name(i as u32).unwrap_or(""));
            h.str(&s.name);
            h.f64(s.position[0]);
            h.f64(s.position[1]);
            h.usize(s.industries.len());
            for r in &s.industries {
                h.u32(r.0);
            }
            h.usize(s.connections.len());
            for c in &s.connections {
                h.u32(c.0);
            }
            h.usize(s.initial_stock.len());
            for (c, q) in &s.initial_stock {
                h.u32(c.0);
                h.u32(*q);
            }
            h.str(&s.pricing);
            h.f64(s.danger);
        }

        // Ships.
        h.usize(self.ships.len());
        for (i, s) in self.ships.iter().enumerate() {
            h.str(self.ship_ids.name(i as u32).unwrap_or(""));
            h.str(&s.id);
            h.str(&s.name);
            h.u32(s.cargo_capacity);
            h.f64(s.jump_speed);
            h.u32(s.hull);
            h.f64(s.max_speed);
            match &s.combat {
                None => h.u32(0),
                Some(cs) => {
                    h.u32(1);
                    h.u32(cs.shield);
                    h.f64(cs.shield_regen);
                    h.u32(cs.weapon_damage);
                    h.f64(cs.weapon_range);
                    h.u32(cs.weapon_cooldown);
                    h.f64(cs.accuracy);
                    h.f64(cs.acceleration);
                }
            }
        }

        // Scripts, in load order: a different script name, kind, or body is a
        // behavioural difference clients and servers must agree on.
        h.usize(self.scripts.len());
        for s in &self.scripts {
            h.str(&s.name);
            h.u32(s.kind as u32);
            h.str(&s.source);
        }

        h.finish()
    }
}

/// A tiny FNV-1a 64-bit hasher used to fingerprint linked content.
///
/// Rolled by hand (rather than via `std`'s `DefaultHasher`) so the fingerprint is
/// a fixed algorithm with a fixed seed: it is stable across Rust versions,
/// platforms, and separate builds, which is what lets an independently-built
/// client and server agree on the hash for identical content. Floats are folded
/// via their bit pattern and every variable-length field is preceded by its
/// length, so distinct content cannot collide by field-boundary ambiguity.
struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Fnv(Self::OFFSET)
    }

    fn byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }

    fn bytes(&mut self, bs: &[u8]) {
        for &b in bs {
            self.byte(b);
        }
    }

    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    fn usize(&mut self, v: usize) {
        self.u64(v as u64);
    }

    fn i64(&mut self, v: i64) {
        self.bytes(&v.to_le_bytes());
    }

    fn f64(&mut self, v: f64) {
        // Normalise -0.0 to 0.0 so two equal values never differ by sign of zero.
        let v = if v == 0.0 { 0.0 } else { v };
        self.u64(v.to_bits());
    }

    fn str(&mut self, s: &str) {
        self.usize(s.len());
        self.bytes(s.as_bytes());
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Link merged content into a [`Registry`], validating every reference.
///
/// `known_pricing` is the set of pricing strategy names the caller has registered
/// (e.g. the economy's built-in strategies). Passing it in keeps this crate
/// ignorant of the economy while still failing fast on a bad `pricing` name.
pub fn link(
    merged: MergedContent,
    known_pricing: &HashSet<String>,
) -> Result<Registry, LoadError> {
    // Intern all ids first so references may point forward or backward freely.
    // Interning in vector order makes handle.index() == vector index.
    let mut commodity_ids = Interner::new();
    for c in &merged.commodities {
        commodity_ids.intern(&c.id);
    }
    let mut recipe_ids = Interner::new();
    for r in &merged.recipes {
        recipe_ids.intern(&r.id);
    }
    let mut system_ids = Interner::new();
    for s in &merged.systems {
        system_ids.intern(&s.id);
    }
    let mut ship_ids = Interner::new();
    for s in &merged.ships {
        ship_ids.intern(&s.id);
    }

    // A mod-declared pricing script registers a name content may select exactly
    // like a built-in. Fold those names into the set accepted for a system's
    // `pricing`, after rejecting any that would shadow a strategy the engine
    // already provides (a silent shadow would make behaviour depend on load order).
    let mut valid_pricing: HashSet<&str> = known_pricing.iter().map(String::as_str).collect();
    for script in &merged.scripts {
        if script.kind == ScriptKind::Pricing {
            if known_pricing.contains(script.name.as_str()) {
                return Err(LoadError::ScriptShadowsBuiltin {
                    mod_id: script.mod_id.clone(),
                    name: script.name.clone(),
                });
            }
            valid_pricing.insert(script.name.as_str());
        }
    }

    let commodity = |referrer: &str, name: &str| -> Result<CommodityId, LoadError> {
        commodity_ids
            .get(name)
            .map(CommodityId)
            .ok_or_else(|| LoadError::DanglingRef {
                kind: "recipe/system",
                referrer: referrer.to_string(),
                target_kind: "commodity",
                target: name.to_string(),
            })
    };

    // Resolve recipes.
    let mut recipes = Vec::with_capacity(merged.recipes.len());
    for r in &merged.recipes {
        let id = RecipeId(recipe_ids.get(&r.id).expect("interned above"));
        let inputs = r
            .inputs
            .iter()
            .map(|a| Ok((commodity(&r.id, &a.commodity)?, a.qty)))
            .collect::<Result<Vec<_>, LoadError>>()?;
        let outputs = r
            .outputs
            .iter()
            .map(|a| Ok((commodity(&r.id, &a.commodity)?, a.qty)))
            .collect::<Result<Vec<_>, LoadError>>()?;
        recipes.push(ResolvedRecipe {
            id,
            inputs,
            outputs,
            rate: r.rate,
            elasticity: r.elasticity,
        });
    }

    // Resolve systems.
    let mut systems = Vec::with_capacity(merged.systems.len());
    for s in &merged.systems {
        let id = SystemId(system_ids.get(&s.id).expect("interned above"));

        let industries = s
            .industries
            .iter()
            .map(|rid| {
                recipe_ids
                    .get(rid)
                    .map(RecipeId)
                    .ok_or_else(|| LoadError::DanglingRef {
                        kind: "system",
                        referrer: s.id.clone(),
                        target_kind: "recipe",
                        target: rid.clone(),
                    })
            })
            .collect::<Result<Vec<_>, LoadError>>()?;

        let connections = s
            .connections
            .iter()
            .map(|sid| {
                system_ids
                    .get(sid)
                    .map(SystemId)
                    .ok_or_else(|| LoadError::DanglingRef {
                        kind: "system",
                        referrer: s.id.clone(),
                        target_kind: "system",
                        target: sid.clone(),
                    })
            })
            .collect::<Result<Vec<_>, LoadError>>()?;

        let initial_stock = s
            .initial_stock
            .iter()
            .map(|a| Ok((commodity(&s.id, &a.commodity)?, a.qty)))
            .collect::<Result<Vec<_>, LoadError>>()?;

        if !valid_pricing.contains(s.pricing.as_str()) {
            return Err(LoadError::UnknownPricing {
                system: s.id.clone(),
                strategy: s.pricing.clone(),
            });
        }

        systems.push(ResolvedSystem {
            id,
            name: s.name.clone(),
            position: s.position,
            industries,
            connections,
            initial_stock,
            pricing: s.pricing.clone(),
            danger: s.danger,
        });
    }

    // Non-fatal hygiene check: warn on one-way jump connections.
    warn_asymmetric_connections(&systems);

    Ok(Registry {
        commodity_ids,
        commodities: merged.commodities,
        recipe_ids,
        recipes,
        system_ids,
        systems,
        ship_ids,
        ships: merged.ships,
        scripts: merged.scripts,
    })
}

/// Warn (not error) if a jump connection is not mirrored. A one-way jump is
/// usually an authoring mistake, but not fatal.
fn warn_asymmetric_connections(systems: &[ResolvedSystem]) {
    let has: HashSet<(u32, u32)> = systems
        .iter()
        .flat_map(|s| s.connections.iter().map(move |c| (s.id.0, c.0)))
        .collect();
    for s in systems {
        for c in &s.connections {
            if !has.contains(&(c.0, s.id.0)) {
                warn!(
                    from = s.id.0,
                    to = c.0,
                    "asymmetric jump connection (not mirrored)"
                );
            }
        }
    }
}

#[cfg(test)]
mod hash_tests {
    use super::*;
    use crate::loader::load;
    use std::path::PathBuf;

    fn mods_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods")
    }

    /// Every pricing name the merged content references, so `link` succeeds
    /// without this crate having to know the economy's strategy registry.
    fn known_pricing(merged: &MergedContent) -> HashSet<String> {
        merged.systems.iter().map(|s| s.pricing.clone()).collect()
    }

    fn link_bundled(merged: MergedContent) -> Registry {
        let known = known_pricing(&merged);
        link(merged, &known).unwrap()
    }

    #[test]
    fn identical_content_hashes_equal() {
        // Two independent loads of the same mods must fingerprint identically —
        // the property the handshake relies on for a matching client and server.
        let a = link_bundled(load(&mods_root()).unwrap());
        let b = link_bundled(load(&mods_root()).unwrap());
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn changed_commodity_price_changes_hash() {
        let base = link_bundled(load(&mods_root()).unwrap()).content_hash();

        let mut merged = load(&mods_root()).unwrap();
        assert!(!merged.commodities.is_empty());
        merged.commodities[0].base_price += 1; // one credit of drift
        let changed = link_bundled(merged).content_hash();

        assert_ne!(base, changed, "a content change must change the fingerprint");
    }

    #[test]
    fn changed_system_danger_changes_hash() {
        let base = link_bundled(load(&mods_root()).unwrap()).content_hash();

        let mut merged = load(&mods_root()).unwrap();
        assert!(!merged.systems.is_empty());
        merged.systems[0].danger += 0.01;
        let changed = link_bundled(merged).content_hash();

        assert_ne!(base, changed);
    }

    #[test]
    fn changed_script_source_changes_hash() {
        // Scripts are behaviour a networked client and server must agree on, so a
        // different script body must change the fingerprint.
        use crate::loader::LoadedScript;
        use crate::manifest::ScriptKind;

        let mut merged = load(&mods_root()).unwrap();
        merged.scripts.push(LoadedScript {
            name: "m:flat".into(),
            kind: ScriptKind::Pricing,
            source: "fn price(base, stock, equilibrium, elasticity) { base }".into(),
            mod_id: "m".into(),
        });
        let known = known_pricing(&merged);
        let with_a = link(merged, &known).unwrap().content_hash();

        let mut merged = load(&mods_root()).unwrap();
        merged.scripts.push(LoadedScript {
            name: "m:flat".into(),
            kind: ScriptKind::Pricing,
            source: "fn price(base, stock, equilibrium, elasticity) { base * 2 }".into(),
            mod_id: "m".into(),
        });
        let known = known_pricing(&merged);
        let with_b = link(merged, &known).unwrap().content_hash();

        assert_ne!(with_a, with_b, "a different script body must change the fingerprint");
    }

    #[test]
    fn reordering_commodities_changes_interning_and_hash() {
        // Interned order is load-bearing (handles are indices). A different order
        // must be caught, because it would desync id resolution.
        let base = link_bundled(load(&mods_root()).unwrap()).content_hash();

        let mut merged = load(&mods_root()).unwrap();
        if merged.commodities.len() >= 2 {
            merged.commodities.swap(0, 1);
            let reordered = link_bundled(merged).content_hash();
            assert_ne!(base, reordered);
        }
    }
}
