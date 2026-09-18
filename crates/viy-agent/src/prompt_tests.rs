//! rpctl's SystemGuideTests and the web-tool guide rule, the system prompt
//! against what rp sent in the recorded scenarios, and the workspace. The
//! whole prompt against rpctl's for many trees is in instructions_tests.

use std::path::Path;

use pretty_assertions::assert_eq;
use viy_testkit::scenario;

use super::*;

fn recorded_system(name: &str) -> String {
    let bodies = scenario::rp_log(name).unwrap().chat_bodies();
    bodies[0]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// DEFAULTS as rpctl's SystemGuideTests make a chat: build tools, guide and
/// explore on, search off, no instruction files.
fn make() -> SystemPrompt<'static> {
    SystemPrompt {
        user: None,
        tools_on: true,
        search_on: false,
        guide_on: true,
        explore_trust: true,
        consult: false,
        toolbox_index: None,
        workspace: Path::new("/w/proj"),
        today: "2026-07-03 (Friday)",
        instructions: None,
    }
}

#[test]
fn default_prompt_has_environment_and_rules() {
    let text = make().render().unwrap();
    assert!(text.starts_with(TOOL_SYSTEM_GUIDE));
    assert!(text.contains("Today is 2026-07-03 (Friday)."));
    assert!(text.contains("bash in /w/proj"));
    for part in [ANSWER_GUIDE, TOOL_WORK_GUIDE, EXPLORE_TRUST_GUIDE] {
        assert!(text.contains(part));
    }
    assert!(!text.contains(SEARCH_GUIDE));
    let search = SystemPrompt {
        search_on: true,
        ..make()
    }
    .render()
    .unwrap();
    assert!(search.contains(SEARCH_GUIDE));
}

#[test]
fn guide_off_is_the_old_prompt() {
    let off = SystemPrompt {
        guide_on: false,
        ..make()
    };
    assert_eq!(off.render().as_deref(), Some(TOOL_SYSTEM_GUIDE));
}

#[test]
fn explore_line_has_its_own_switch() {
    let text = SystemPrompt {
        explore_trust: false,
        ..make()
    }
    .render()
    .unwrap();
    assert!(text.contains(TOOL_WORK_GUIDE));
    assert!(!text.contains(EXPLORE_TRUST_GUIDE));
}

#[test]
fn without_tools_only_the_answer_rules() {
    let text = SystemPrompt {
        tools_on: false,
        search_on: true,
        ..make()
    }
    .render()
    .unwrap();
    assert!(text.contains(ANSWER_GUIDE));
    assert!(!text.contains("run_shell"));
    assert!(!text.contains(TOOL_WORK_GUIDE));
    assert!(text.ends_with(&SEARCH_GUIDE[2..]));
}

#[test]
fn tools_and_guide() {
    // rpctl's ChatWebTests: the untrusted-text rule follows web search, not
    // SYSTEM_GUIDE.
    let guide_off = SystemPrompt {
        search_on: true,
        guide_on: false,
        ..make()
    };
    assert!(guide_off.render().unwrap().contains(UNTRUSTED_GUIDE));
    assert!(!make().render().unwrap().contains(UNTRUSTED_GUIDE));
}

#[test]
fn the_toolbox_and_ask_claude_bring_their_lines() {
    let text = SystemPrompt {
        consult: true,
        toolbox_index: Some("# Toolbox\n- a-tool: does a"),
        search_on: true,
        ..make()
    }
    .render()
    .unwrap();
    let work = format!(
        "{TOOL_WORK_GUIDE}\n{EXPLORE_TRUST_GUIDE}\n{CONSULT_GUIDE}\n{TOOLBOX_GUIDE}\n{SEARCH_GUIDE}"
    );
    assert!(text.ends_with(&format!("{work}\n\n# Toolbox\n- a-tool: does a")));
    let no_tools = SystemPrompt {
        tools_on: false,
        toolbox_index: Some("# Toolbox"),
        ..make()
    };
    assert!(!no_tools.render().unwrap().contains("Toolbox"));
}

#[test]
fn equals_what_rp_sent_with_search_on_and_off() {
    let base = SystemPrompt {
        tools_on: false,
        explore_trust: false,
        today: "{TODAY}",
        ..make()
    };
    assert_eq!(
        SystemPrompt {
            search_on: true,
            ..base.clone()
        }
        .render()
        .unwrap(),
        recorded_system("plain-two-turns")
    );
    assert_eq!(
        base.render().unwrap(),
        recorded_system("length-cut-and-retry")
    );
}

#[test]
fn the_workspace_is_the_project_unless_set() {
    let mut cfg = Config::defaults();
    let (project, home) = (Path::new("/p"), Path::new("/h"));
    assert_eq!(workspace(&cfg, project, home), Path::new("/p"));
    for (set, want) in [
        ("ws", "/p/ws"),
        (" /abs ", "/abs"),
        ("~", "/h"),
        ("~/code", "/h/code"),
        ("~other/x", "/p/~other/x"),
        ("  ", "/p"),
    ] {
        cfg.set("WORKSPACE_DIR", set);
        assert_eq!(workspace(&cfg, project, home), Path::new(want), "{set:?}");
    }
}

#[test]
fn today_names_the_weekday() {
    let today = today();
    let (date, weekday) = today.split_once(' ').unwrap();
    assert_eq!(date.len(), 10);
    assert!(
        weekday.starts_with('(') && weekday.ends_with("day)"),
        "{today}"
    );
}
