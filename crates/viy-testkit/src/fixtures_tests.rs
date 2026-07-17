use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn every_fixture_is_from_the_pinned_commit() {
    let names = all().unwrap();
    assert!(names.contains(&"config/layering".to_string()), "{names:?}");
    for name in names {
        let fixture = load(&name).unwrap();
        assert!(!fixture.cases.is_empty(), "{name}: no cases");
    }
}

#[test]
fn config_layering_cases_keep_the_defaults_order() {
    // Not the layering itself, which arrives with viy-config (WP 1.2): this
    // checks that the replayed data carries what that replay needs, an
    // ordered map per case that starts with DEFAULTS in rpctl's order.
    let fixture = load("config/layering").unwrap();
    let defaults: Vec<&String> = fixture.case("defaults_only").unwrap()["config"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(defaults.len(), 110);
    assert_eq!(defaults.first().map(|k| k.as_str()), Some("MODEL_REPO"));
    for case in &fixture.cases {
        let keys: Vec<&String> = case["config"].as_object().unwrap().keys().collect();
        assert_eq!(keys[..defaults.len()], defaults[..], "{}", case["name"]);
    }
}

#[test]
fn config_layering_records_python_strip() {
    let fixture = load("config/layering").unwrap();
    let config = &fixture.case("global_file_syntax").unwrap()["config"];
    // str.strip() removes \x1f; Rust's trim() would not (flags.md F05).
    assert_eq!(config["ENDPOINT_MODEL"], json!("unit-sep"));
    assert_eq!(config["export TUNNEL"], json!("0"));
}
