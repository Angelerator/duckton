//! Guards the checked-in example config against drift: it must parse with
//! `deny_unknown_fields` and pass validation.

use std::path::PathBuf;

use p2p_config::GridConfig;

fn example_path() -> PathBuf {
    // crates/config/ -> repo root -> config/p2p.example.toml
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/p2p.example.toml")
}

#[test]
fn example_config_parses_and_validates() {
    let path = example_path();
    let cfg = GridConfig::from_toml_file(&path)
        .unwrap_or_else(|e| panic!("example config {} failed to parse: {e}", path.display()));
    cfg.validate().expect("example config must validate");
}

#[test]
fn example_config_matches_documented_defaults() {
    // The example file documents the defaults; loading it should equal the
    // built-in default config. This keeps docs and code in lock-step.
    let cfg = GridConfig::from_toml_file(&example_path()).unwrap();
    assert_eq!(cfg, GridConfig::default());
}
