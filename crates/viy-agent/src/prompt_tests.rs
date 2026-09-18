//! The system prompt against what rp sent in the recorded scenarios, and
//! which parts each setting brings.

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

fn prompt(search_on: bool, guide_on: bool) -> SystemPrompt<'static> {
    SystemPrompt {
        user: None,
        search_on,
        guide_on,
        today: "{TODAY}",
    }
}

#[test]
fn equals_what_rp_sent_with_search_on_and_off() {
    assert_eq!(
        prompt(true, true).render().unwrap(),
        recorded_system("plain-two-turns")
    );
    assert_eq!(
        prompt(false, true).render().unwrap(),
        recorded_system("length-cut-and-retry")
    );
}

#[test]
fn each_setting_brings_its_parts() {
    assert_eq!(prompt(false, false).render(), None);
    assert_eq!(
        prompt(true, false).render().as_deref(),
        Some(UNTRUSTED_GUIDE)
    );
    let mine = SystemPrompt {
        user: Some("Answer in French."),
        ..prompt(true, false)
    };
    assert_eq!(
        mine.render().unwrap(),
        format!("Answer in French.\n\n{UNTRUSTED_GUIDE}")
    );
    let empty = SystemPrompt {
        user: Some(""),
        ..prompt(false, false)
    };
    assert_eq!(empty.render(), None);
    let guided = prompt(false, true).render().unwrap();
    assert!(guided.starts_with("Today is {TODAY}. The user's machine runs Linux.\n\nHow to help:"));
    assert!(!guided.contains("web_search"));
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
