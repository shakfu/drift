//! Loader + linker behavior against on-disk fixture mods.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use drift_mods::{load_and_link, LoadError};
use tempfile::TempDir;

fn pricing_set() -> HashSet<String> {
    HashSet::from(["supply_demand_v1".to_string()])
}

/// Write a single content file `<mod>/<subdir>/<name>.ron` with the given body.
fn write_content(root: &Path, mod_id: &str, subdir: &str, name: &str, body: &str) {
    let dir = root.join(mod_id).join(subdir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.ron")), body).unwrap();
}

fn write_manifest(root: &Path, mod_id: &str, body: &str) {
    let dir = root.join(mod_id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("manifest.toml"), body).unwrap();
}

/// A minimal, internally-consistent mod named `core`.
fn write_valid_core(root: &Path) {
    write_manifest(
        root,
        "core",
        r#"id = "core"
name = "Core"
version = "0.1.0"
"#,
    );
    write_content(
        root,
        "core",
        "commodities",
        "goods",
        r#"[
            (id: "core:food", name: "Food", base_price: 100, unit_mass: 1, elasticity: 0.8, category: "food"),
            (id: "core:ore",  name: "Ore",  base_price: 40,  unit_mass: 2, elasticity: 0.9, category: "minerals"),
        ]"#,
    );
    write_content(
        root,
        "core",
        "production",
        "recipes",
        r#"[
            (id: "core:mine_ore", inputs: [], outputs: [(commodity: "core:ore", qty: 5)], rate: 3),
        ]"#,
    );
    write_content(
        root,
        "core",
        "systems",
        "lave",
        r#"[
            (id: "core:lave", name: "Lave", position: (0.0, 0.0),
             industries: ["core:mine_ore"], connections: [],
             initial_stock: [(commodity: "core:food", qty: 100), (commodity: "core:ore", qty: 50)],
             pricing: "supply_demand_v1"),
        ]"#,
    );
    write_content(
        root,
        "core",
        "ships",
        "cobra",
        r#"[
            (id: "core:cobra_mk3", name: "Cobra Mk III", cargo_capacity: 35, jump_speed: 7.0, hull: 100, max_speed: 350.0),
        ]"#,
    );
}

#[test]
fn valid_mod_loads_and_links() {
    let tmp = TempDir::new().unwrap();
    write_valid_core(tmp.path());

    let reg = load_and_link(tmp.path(), &pricing_set()).expect("should link");
    assert_eq!(reg.commodity_count(), 2);
    assert_eq!(reg.system_count(), 1);
    assert!(reg.commodity_id("core:food").is_some());
    assert!(reg.ship_id("core:cobra_mk3").is_some());
    // The system's industry resolved to the mining recipe.
    let sys = reg.systems().next().unwrap();
    assert_eq!(sys.industries.len(), 1);
    assert_eq!(reg.recipe(sys.industries[0]).outputs.len(), 1);
}

#[test]
fn missing_dependency_errors() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "a",
        r#"id = "a"
name = "A"
version = "0.1.0"
dependencies = ["b"]
"#,
    );
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(
        matches!(err, LoadError::MissingDependency { ref mod_id, ref dependency } if mod_id == "a" && dependency == "b"),
        "got {err:?}"
    );
}

#[test]
fn dependency_cycle_errors() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "a",
        "id = \"a\"\nname = \"A\"\nversion = \"0.1.0\"\ndependencies = [\"b\"]\n",
    );
    write_manifest(
        tmp.path(),
        "b",
        "id = \"b\"\nname = \"B\"\nversion = \"0.1.0\"\ndependencies = [\"a\"]\n",
    );
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(matches!(err, LoadError::DependencyCycle(_)), "got {err:?}");
}

#[test]
fn dangling_commodity_reference_errors() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "core",
        "id = \"core\"\nname = \"Core\"\nversion = \"0.1.0\"\n",
    );
    // Recipe outputs a commodity that is never defined.
    write_content(
        tmp.path(),
        "core",
        "production",
        "recipes",
        r#"[ (id: "core:bad", inputs: [], outputs: [(commodity: "core:ghost", qty: 1)], rate: 1) ]"#,
    );
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(
        matches!(err, LoadError::DanglingRef { ref target, .. } if target == "core:ghost"),
        "got {err:?}"
    );
}

#[test]
fn unknown_pricing_strategy_errors() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "core",
        "id = \"core\"\nname = \"Core\"\nversion = \"0.1.0\"\n",
    );
    write_content(
        tmp.path(),
        "core",
        "commodities",
        "goods",
        r#"[ (id: "core:food", name: "Food", base_price: 100, unit_mass: 1, elasticity: 0.8, category: "food") ]"#,
    );
    write_content(
        tmp.path(),
        "core",
        "systems",
        "lave",
        r#"[ (id: "core:lave", name: "Lave", position: (0.0, 0.0), industries: [], connections: [],
             initial_stock: [(commodity: "core:food", qty: 10)], pricing: "does_not_exist") ]"#,
    );
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(
        matches!(err, LoadError::UnknownPricing { ref strategy, .. } if strategy == "does_not_exist"),
        "got {err:?}"
    );
}

#[test]
fn duplicate_id_without_override_errors() {
    let tmp = TempDir::new().unwrap();
    write_valid_core(tmp.path());
    // A second mod redefines core:food but does NOT declare an override.
    write_manifest(
        tmp.path(),
        "patch",
        "id = \"patch\"\nname = \"Patch\"\nversion = \"0.1.0\"\ndependencies = [\"core\"]\n",
    );
    write_content(
        tmp.path(),
        "patch",
        "commodities",
        "food",
        r#"[ (id: "core:food", name: "Food+", base_price: 200, unit_mass: 1, elasticity: 0.8, category: "food") ]"#,
    );
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(
        matches!(err, LoadError::DuplicateId { ref id, .. } if id == "core:food"),
        "got {err:?}"
    );
}

#[test]
fn declared_override_replaces_definition() {
    let tmp = TempDir::new().unwrap();
    write_valid_core(tmp.path());
    write_manifest(
        tmp.path(),
        "patch",
        "id = \"patch\"\nname = \"Patch\"\nversion = \"0.1.0\"\ndependencies = [\"core\"]\noverrides = [\"core:food\"]\n",
    );
    write_content(
        tmp.path(),
        "patch",
        "commodities",
        "food",
        r#"[ (id: "core:food", name: "Food+", base_price: 200, unit_mass: 1, elasticity: 0.8, category: "food") ]"#,
    );
    let reg = load_and_link(tmp.path(), &pricing_set()).expect("override should link");
    let food = reg.commodity_id("core:food").unwrap();
    assert_eq!(reg.commodity(food).base_price, 200, "override took effect");
    assert_eq!(reg.commodity(food).name, "Food+");
    // Override replaces in place; the commodity count is unchanged.
    assert_eq!(reg.commodity_count(), 2);
}

/// Write a `.rhai` script file at `<mod>/<rel>`.
fn write_script(root: &Path, mod_id: &str, rel: &str, body: &str) {
    let path = root.join(mod_id).join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A pricing script named `test:flat` that a system in `core` selects.
const FLAT_PRICING: &str =
    "fn price(base, stock, equilibrium, elasticity) { base }";

/// `core` plus a `[[scripts]]` entry, with the system pointing `pricing` at it.
fn write_core_with_script(root: &Path) {
    write_manifest(
        root,
        "core",
        r#"id = "core"
name = "Core"
version = "0.1.0"

[[scripts]]
name = "test:flat"
path = "scripts/flat.rhai"
kind = "pricing"
"#,
    );
    write_script(root, "core", "scripts/flat.rhai", FLAT_PRICING);
    write_content(
        root,
        "core",
        "commodities",
        "goods",
        r#"[ (id: "core:food", name: "Food", base_price: 100, unit_mass: 1, elasticity: 0.8, category: "food") ]"#,
    );
    write_content(
        root,
        "core",
        "systems",
        "lave",
        r#"[
            (id: "core:lave", name: "Lave", position: (0.0, 0.0),
             industries: [], connections: [],
             initial_stock: [(commodity: "core:food", qty: 100)],
             pricing: "test:flat"),
        ]"#,
    );
}

#[test]
fn a_system_may_name_a_mod_declared_pricing_script() {
    let tmp = TempDir::new().unwrap();
    write_core_with_script(tmp.path());

    // `test:flat` is not a built-in, yet linking succeeds because the loader folds
    // the mod-declared script name into the accepted pricing set.
    let reg = load_and_link(tmp.path(), &pricing_set()).expect("scripted pricing should link");
    assert_eq!(reg.scripts().len(), 1);
    let s = &reg.scripts()[0];
    assert_eq!(s.name, "test:flat");
    assert_eq!(s.mod_id, "core");
    assert_eq!(s.source, FLAT_PRICING);
    assert_eq!(reg.systems().next().unwrap().pricing, "test:flat");
}

#[test]
fn a_missing_script_file_fails_the_load() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "core",
        r#"id = "core"
name = "Core"
version = "0.1.0"

[[scripts]]
name = "test:flat"
path = "scripts/missing.rhai"
"#,
    );
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(matches!(err, LoadError::Io { .. }), "got {err:?}");
}

#[test]
fn a_script_shadowing_a_builtin_is_rejected() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "core",
        r#"id = "core"
name = "Core"
version = "0.1.0"

[[scripts]]
name = "supply_demand_v1"
path = "scripts/flat.rhai"
"#,
    );
    write_script(tmp.path(), "core", "scripts/flat.rhai", FLAT_PRICING);
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(
        matches!(err, LoadError::ScriptShadowsBuiltin { ref name, .. } if name == "supply_demand_v1"),
        "got {err:?}"
    );
}

#[test]
fn duplicate_script_names_across_mods_are_rejected() {
    let tmp = TempDir::new().unwrap();
    for m in ["a", "b"] {
        write_manifest(
            tmp.path(),
            m,
            &format!(
                r#"id = "{m}"
name = "{m}"
version = "0.1.0"

[[scripts]]
name = "shared:flat"
path = "scripts/flat.rhai"
"#
            ),
        );
        write_script(tmp.path(), m, "scripts/flat.rhai", FLAT_PRICING);
    }
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(
        matches!(err, LoadError::DuplicateId { kind: "script", ref id, .. } if id == "shared:flat"),
        "got {err:?}"
    );
}

#[test]
fn an_unknown_script_kind_fails_to_parse() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "core",
        r#"id = "core"
name = "Core"
version = "0.1.0"

[[scripts]]
name = "test:flat"
path = "scripts/flat.rhai"
kind = "trader_ai"
"#,
    );
    write_script(tmp.path(), "core", "scripts/flat.rhai", FLAT_PRICING);
    let err = load_and_link(tmp.path(), &pricing_set()).unwrap_err();
    assert!(matches!(err, LoadError::ManifestParse { .. }), "got {err:?}");
}
