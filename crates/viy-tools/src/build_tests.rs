use pretty_assertions::assert_eq;
use serde_json::Value;

use super::*;

/// The `chat/tools` fixture: rpctl's `_tool_specs()` per config.
fn rpctl(case: &str) -> Value {
    let fixture = viy_testkit::fixtures::load("chat/tools").unwrap();
    fixture.case(case).unwrap().clone()
}

/// Key order matters (the template renders with tojson), so compare the
/// serialized text.
fn text(v: &[Value]) -> String {
    serde_json::to_string(v).unwrap()
}

#[test]
fn build_tools_are_rpctls() {
    let theirs = rpctl("build_tools")["tools"].as_array().unwrap().clone();
    assert_eq!(text(&tools(false)), text(&theirs));
    let theirs = rpctl("build_tools_host_net")["tools"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(text(&tools(true)), text(&theirs));
}

#[test]
fn web_tools_come_first_as_rpctl_orders_them() {
    let theirs = rpctl("build_and_web_tools")["tools"]
        .as_array()
        .unwrap()
        .clone();
    let mut ours = vec![crate::web::search_tool(), crate::web::fetch_tool()];
    ours.extend(tools(false));
    assert_eq!(text(&ours), text(&theirs));
}

#[test]
fn explore_comes_after_the_build_tools() {
    let theirs = rpctl("with_explore")["tools"].as_array().unwrap().clone();
    let mut ours = vec![crate::web::search_tool(), crate::web::fetch_tool()];
    ours.extend(tools(false));
    ours.push(crate::explore::explore_tool());
    assert_eq!(text(&ours), text(&theirs));
}

#[test]
fn names_and_parallel_safe_tools_are_rpctls() {
    let n = rpctl("notices");
    let list = |k: &str| -> Vec<String> {
        n[k].as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(list("build_tool_names"), NAMES);
    assert_eq!(list("parallel_safe"), PARALLEL_SAFE);
}
