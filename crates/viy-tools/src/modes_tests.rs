use pretty_assertions::assert_eq;
use viy_protocol::QuestionKind;

use super::*;

fn auto() -> &'static Mode {
    get("auto-yes").unwrap()
}

// rpctl tests/test_modes.py CarouselTests, ported (the chat's parts are in
// viy-agent's engine and command tests).

#[test]
fn wraps_around_in_list_order() {
    let mut m = DEFAULT;
    let mut seen = Vec::new();
    for _ in 0..MODES.len() {
        seen.push(m.name);
        m = after(m);
    }
    assert_eq!(seen, MODES.iter().map(|x| x.name).collect::<Vec<_>>());
    assert_eq!(m, DEFAULT);
}

#[test]
fn kinds_are_known() {
    for m in &MODES {
        for k in m.auto_yes {
            assert!(KINDS.contains(k), "{}", m.name);
        }
    }
}

#[test]
fn auto_yes_covers_the_internet() {
    for k in [
        QuestionKind::Network,
        QuestionKind::Fetch,
        QuestionKind::Remote,
    ] {
        assert!(auto().answers(Some(k)), "{k:?}");
    }
}

#[test]
fn auto_yes_keeps_asking_about_client_files_and_the_host() {
    for k in [QuestionKind::ProtectedWrite, QuestionKind::Unsandboxed] {
        assert!(!auto().answers(Some(k)), "{k:?}");
    }
    assert!(!auto().answers(None), "a question without a kind waits");
}

#[test]
fn names_are_found_case_and_spaces_ignored() {
    assert_eq!(get("Auto-Yes "), Some(auto()));
    assert_eq!(get(" normal"), Some(DEFAULT));
    assert_eq!(get("bogus"), None);
    assert_eq!(get(""), None);
}

#[test]
fn lines_and_kind_words_are_rps() {
    assert_eq!(
        DEFAULT.line(),
        "normal -- every question waits for your answer (shift+tab to cycle)"
    );
    assert!(
        auto()
            .line()
            .starts_with("⏵⏵ auto-yes -- says yes to destructive commands")
    );
    assert_eq!(kind_words(QuestionKind::SecretRead), "secret read");
    assert_eq!(kind_words(QuestionKind::ProtectedWrite), "protected write");
}

/// The table equals rpctl's modes.py (the `chat/modes` fixture): kinds,
/// names, labels, blurbs, what each answers, `get` and `after`. rp's ANSI
/// colour (yellow for auto-yes) is the UI's (viy-term's footer).
#[test]
fn the_table_is_rpctls() {
    let fixture = viy_testkit::fixtures::load("chat/modes").unwrap();
    let case = fixture.case("modes").unwrap();
    let name = |k: &QuestionKind| serde_json::to_value(k).unwrap();
    assert_eq!(
        KINDS.iter().map(name).collect::<Vec<_>>(),
        case["kinds"].as_array().unwrap().clone()
    );
    let theirs = case["modes"].as_array().unwrap();
    assert_eq!(theirs.len(), MODES.len());
    for (m, t) in MODES.iter().zip(theirs) {
        assert_eq!(m.name, t["name"]);
        assert_eq!(m.label, t["label"]);
        assert_eq!(m.blurb, t["blurb"]);
        let ours: Vec<_> = KINDS
            .iter()
            .filter(|k| m.auto_yes.contains(k))
            .map(name)
            .collect();
        assert_eq!(&ours, t["auto_yes"].as_array().unwrap(), "{}", m.name);
    }
    assert_eq!(DEFAULT.name, case["default"]);
    for pair in case["get"].as_array().unwrap() {
        let got = get(pair[0].as_str().unwrap()).map(|m| m.name);
        assert_eq!(got, pair[1].as_str(), "{}", pair[0]);
    }
    for pair in case["after"].as_array().unwrap() {
        let m = get(pair[0].as_str().unwrap()).unwrap();
        assert_eq!(after(m).name, pair[1]);
    }
}
