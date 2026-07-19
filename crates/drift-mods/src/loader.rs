//! Discovery, dependency ordering, content loading, and merge.
//!
//! Pipeline: discover mod dirs -> topologically order them by dependency ->
//! load each mod's RON content -> merge into a single id-keyed set applying
//! override rules. The output ([`MergedContent`]) is still string-id based; it is
//! [`link`](crate::registry::link)ed into handle-based runtime data separately.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use drift_data::{CommodityDef, ProductionRecipe, ShipDef, SystemDef};
use serde::de::DeserializeOwned;
use tracing::debug;

use crate::error::LoadError;
use crate::manifest::{Manifest, ModDir, ScriptKind};

/// A mod-declared behavior script, read from disk: the name it registers, the
/// seam it plugs into, and its `.rhai` source.
///
/// Kept as *source* rather than a compiled artefact so `drift-mods` stays free of
/// the scripting engine (the loader validates content; the host compiles and runs
/// scripts). The `mod_id` is retained for diagnostics.
#[derive(Debug, Clone)]
pub struct LoadedScript {
    pub name: String,
    pub kind: ScriptKind,
    pub source: String,
    pub mod_id: String,
}

/// All content merged across mods, in deterministic load order, still keyed by
/// string ids (not yet linked to runtime handles).
#[derive(Debug, Default)]
pub struct MergedContent {
    pub commodities: Vec<CommodityDef>,
    pub recipes: Vec<ProductionRecipe>,
    pub systems: Vec<SystemDef>,
    pub ships: Vec<ShipDef>,
    pub scripts: Vec<LoadedScript>,
}

/// Discover every `manifest.toml` directly under `root`'s subdirectories.
fn discover(root: &Path) -> Result<Vec<ModDir>, LoadError> {
    let mut mods = Vec::new();
    let entries = std::fs::read_dir(root).map_err(|source| LoadError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    // Collect + sort so discovery order is stable regardless of filesystem order.
    let mut dirs: Vec<_> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| LoadError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    for dir in dirs {
        let manifest_path = dir.join("manifest.toml");
        if manifest_path.is_file() {
            let manifest = Manifest::from_path(&manifest_path)?;
            debug!(mod_id = %manifest.id, dir = %dir.display(), "discovered mod");
            mods.push(ModDir { manifest, dir });
        }
    }
    Ok(mods)
}

/// Order mods so every dependency precedes its dependents (Kahn's algorithm).
/// Ties are broken by sorted id, so the order is fully deterministic. Errors on
/// a missing dependency or a cycle.
fn topo_order(mods: Vec<ModDir>) -> Result<Vec<ModDir>, LoadError> {
    let by_id: BTreeMap<String, ModDir> =
        mods.into_iter().map(|m| (m.manifest.id.clone(), m)).collect();

    // Validate dependencies exist up front for a precise error.
    for m in by_id.values() {
        for dep in &m.manifest.dependencies {
            if !by_id.contains_key(dep) {
                return Err(LoadError::MissingDependency {
                    mod_id: m.manifest.id.clone(),
                    dependency: dep.clone(),
                });
            }
        }
    }

    // indegree = number of unmet dependencies.
    let mut indegree: BTreeMap<&str, usize> = by_id
        .values()
        .map(|m| (m.manifest.id.as_str(), m.manifest.dependencies.len()))
        .collect();

    // dependency -> dependents (who to decrement when the dependency is placed).
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in by_id.values() {
        for dep in &m.manifest.dependencies {
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(m.manifest.id.as_str());
        }
    }

    // Ready set kept sorted (BTree keys) for deterministic tie-breaking.
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();
    ready.sort();

    let mut order: Vec<String> = Vec::with_capacity(by_id.len());
    while let Some(id) = ready.pop() {
        order.push(id.to_string());
        if let Some(deps) = dependents.get(id) {
            let mut newly_ready = Vec::new();
            for &dependent in deps {
                let d = indegree.get_mut(dependent).expect("known id");
                *d -= 1;
                if *d == 0 {
                    newly_ready.push(dependent);
                }
            }
            // Re-sort so pops remain in a stable, descending-id order.
            ready.extend(newly_ready);
            ready.sort();
        }
    }

    if order.len() != by_id.len() {
        let mut cyclic: Vec<&str> = indegree
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&id, _)| id)
            .collect();
        cyclic.sort();
        return Err(LoadError::DependencyCycle(cyclic.join(", ")));
    }

    // `order` already lists dependencies before dependents (Kahn's invariant);
    // among independent mods the sorted `ready` set makes ties deterministic.
    let mut by_id = by_id;
    Ok(order
        .into_iter()
        .map(|id| by_id.remove(&id).expect("known id"))
        .collect())
}

/// Load every `*.ron` file in `subdir` (if it exists) as a RON list of `T`.
fn load_dir<T: DeserializeOwned>(dir: &Path, subdir: &str) -> Result<Vec<T>, LoadError> {
    let path = dir.join(subdir);
    if !path.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<_> = std::fs::read_dir(&path)
        .map_err(|source| LoadError::Io {
            path: path.clone(),
            source,
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "ron"))
        .collect();
    files.sort(); // deterministic within a mod

    let mut out = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|source| LoadError::Io {
            path: file.clone(),
            source,
        })?;
        let items: Vec<T> = ron::from_str(&text).map_err(|source| LoadError::ContentParse {
            path: file.clone(),
            source,
        })?;
        out.extend(items);
    }
    Ok(out)
}

/// Merge a per-mod stream of defs into a single id-keyed list, applying override
/// rules. `id_of` extracts the string id; `kind` names the def type for errors.
fn merge<T, F>(
    kind: &'static str,
    per_mod: Vec<(String, &Manifest, Vec<T>)>,
    id_of: F,
) -> Result<Vec<T>, LoadError>
where
    F: Fn(&T) -> &str,
{
    // id -> (position in output, defining mod id)
    let mut index: HashMap<String, (usize, String)> = HashMap::new();
    let mut out: Vec<T> = Vec::new();

    for (mod_id, manifest, defs) in per_mod {
        for def in defs {
            let id = id_of(&def).to_string();
            match index.get(&id) {
                None => {
                    index.insert(id, (out.len(), mod_id.clone()));
                    out.push(def);
                }
                Some((pos, first_mod)) => {
                    // Same mod redefining its own id is always an error; a later
                    // mod may replace only if it opted in via `overrides`.
                    if first_mod == &mod_id || !manifest.overrides(&id) {
                        return Err(LoadError::DuplicateId {
                            kind,
                            id,
                            first: first_mod.clone(),
                            second: mod_id.clone(),
                        });
                    }
                    let pos = *pos;
                    out[pos] = def;
                    index.insert(id, (pos, mod_id.clone()));
                }
            }
        }
    }
    Ok(out)
}

/// Full load: discover, order, read content, merge.
pub fn load(root: &Path) -> Result<MergedContent, LoadError> {
    let mods = topo_order(discover(root)?)?;

    let mut commodities_per_mod = Vec::new();
    let mut recipes_per_mod = Vec::new();
    let mut systems_per_mod = Vec::new();
    let mut ships_per_mod = Vec::new();

    // Scripts declared in manifests, read from disk. Names must be unique across
    // all mods (a collision is as fatal as a duplicate content id). Scripts do not
    // participate in the content `overrides` mechanism.
    let mut scripts: Vec<LoadedScript> = Vec::new();
    let mut script_owner: HashMap<String, String> = HashMap::new();

    for m in &mods {
        commodities_per_mod.push((
            m.manifest.id.clone(),
            &m.manifest,
            load_dir::<CommodityDef>(&m.dir, "commodities")?,
        ));
        recipes_per_mod.push((
            m.manifest.id.clone(),
            &m.manifest,
            load_dir::<ProductionRecipe>(&m.dir, "production")?,
        ));
        systems_per_mod.push((
            m.manifest.id.clone(),
            &m.manifest,
            load_dir::<SystemDef>(&m.dir, "systems")?,
        ));
        ships_per_mod.push((
            m.manifest.id.clone(),
            &m.manifest,
            load_dir::<ShipDef>(&m.dir, "ships")?,
        ));

        for entry in &m.manifest.scripts {
            if let Some(first) = script_owner.get(&entry.name) {
                return Err(LoadError::DuplicateId {
                    kind: "script",
                    id: entry.name.clone(),
                    first: first.clone(),
                    second: m.manifest.id.clone(),
                });
            }
            let path = m.dir.join(&entry.path);
            let source = std::fs::read_to_string(&path).map_err(|source| LoadError::Io {
                path: path.clone(),
                source,
            })?;
            script_owner.insert(entry.name.clone(), m.manifest.id.clone());
            scripts.push(LoadedScript {
                name: entry.name.clone(),
                kind: entry.kind,
                source,
                mod_id: m.manifest.id.clone(),
            });
        }
    }

    Ok(MergedContent {
        commodities: merge("commodity", commodities_per_mod, |c| c.id.as_str())?,
        recipes: merge("recipe", recipes_per_mod, |r| r.id.as_str())?,
        systems: merge("system", systems_per_mod, |s| s.id.as_str())?,
        ships: merge("ship", ships_per_mod, |s| s.id.as_str())?,
        scripts,
    })
}
