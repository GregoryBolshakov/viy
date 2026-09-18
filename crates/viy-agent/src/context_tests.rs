//! The accounting against rpctl's own numbers for the cases of
//! `tests/fixtures/chat/context.json`: estimates, calibration runs and
//! budgets. The context state with the system prompt is checked in
//! chat_tests, where the chat builds it.

use pretty_assertions::assert_eq;
use viy_config::Config;
use viy_testkit::fixtures;

use super::*;

fn config(case: &Value) -> Config {
    let mut cfg = Config::defaults();
    for (key, value) in case["config"].as_object().into_iter().flatten() {
        cfg.set(key, value.as_str().unwrap());
    }
    cfg
}

/// The tools a chat with `cfg` offers, as `Chat::tool_specs` gives them.
pub(crate) fn specs(cfg: &Config) -> Vec<Value> {
    let mut specs = Vec::new();
    if cfg.get("SEARCH_ENABLED") == Some("1") {
        specs.push(viy_tools::web::search_tool());
        if cfg.get("FETCH_ENABLED") != Some("0") {
            specs.push(viy_tools::web::fetch_tool());
        }
    }
    specs
}

fn messages(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn cases(kind: &str) -> Vec<Value> {
    fixtures::load("chat/context")
        .unwrap()
        .cases
        .into_iter()
        .filter(|c| c["kind"] == kind)
        .collect()
}

#[test]
fn estimates_equal_rpctl() {
    let cases = cases("estimates");
    assert_eq!(cases.len(), 18);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let msgs = messages(&case["messages"]);
        let schemas = schema_chars(&specs(&config(&case)));
        let (text, images) = measure(&msgs);
        let estimator = Estimator::default();
        assert_eq!(
            (
                vec![text, images],
                schemas,
                estimator.tokens(&msgs, schemas),
                estimator.tokens(&msgs, 0)
            ),
            (
                vec![
                    case["measure"][0].as_u64().unwrap(),
                    case["measure"][1].as_u64().unwrap()
                ],
                case["schema_chars"].as_u64().unwrap(),
                case["with_schemas"].as_u64().unwrap(),
                case["without_schemas"].as_u64().unwrap()
            ),
            "{name}"
        );
    }
}

#[test]
fn calibration_equals_rpctl_step_by_step() {
    for case in cases("calibrations") {
        let name = case["name"].as_str().unwrap();
        let schemas = schema_chars(&specs(&config(&case)));
        let mut estimator = Estimator::default();
        let mut seen = Vec::new();
        for step in case["steps"].as_array().unwrap() {
            estimator.calibrate(
                &messages(&step["messages"]),
                schemas,
                step["prompt_tokens"].as_i64(),
            );
            seen.push(estimator.chars_per_token);
        }
        let want: Vec<f64> = case["chars_per_token"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        assert_eq!(seen, want, "{name}");
        let first = messages(&case["steps"][0]["messages"]);
        assert_eq!(
            estimator.tokens(&first, schemas),
            case["estimate_after"].as_u64().unwrap(),
            "{name}"
        );
    }
}

#[test]
fn budgets_equal_rpctl() {
    for case in cases("budgets") {
        let cfg = config(&case);
        let max_model_len = crate::chat::max_model_len(&cfg);
        let max_tokens: i64 = cfg.get("MAX_TOKENS").unwrap().trim().parse().unwrap();
        assert_eq!(
            (max_model_len, prompt_budget(max_model_len, max_tokens)),
            (
                case["max_model_len"].as_u64().unwrap(),
                case["budget"].as_u64().unwrap()
            ),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn the_template_spaces_the_schemas() {
    assert_eq!(schema_chars(&[]), 0);
    let one = serde_json::json!({"a": [1, 2], "b": {"c": "é"}});
    // {"a": [1, 2], "b": {"c": "é"}}
    assert_eq!(schema_chars(&[one]), 32);
}
