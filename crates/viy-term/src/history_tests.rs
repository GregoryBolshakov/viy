use std::os::unix::fs::PermissionsExt;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn loaded(entries: &[&str]) -> History {
    let mut history = History::default();
    for e in entries {
        history.record(e).unwrap();
    }
    history
}

#[test]
fn entries_survive_a_restart_one_json_line_each() {
    let dir = ScratchDir::new("history").unwrap();
    let path = dir.path().join(".viy").join("history");
    let mut history = History::load(path.clone());
    history.record("first").unwrap();
    history.record("two\nlines").unwrap();
    history.record("two\nlines").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    for line in text.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(v["ts"].as_u64().is_some(), "{line}");
    }
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let again = History::load(path);
    assert_eq!(again.entries(), ["first", "two\nlines"]);
}

#[test]
fn a_missing_or_broken_file_is_an_empty_history() {
    let dir = ScratchDir::new("history").unwrap();
    assert!(History::load(dir.path().join("none")).entries().is_empty());
    let path = dir.path().join("history");
    std::fs::write(
        &path,
        "not json\n{\"ts\":1,\"text\":\"kept\"}\n{\"ts\":2}\n",
    )
    .unwrap();
    assert_eq!(History::load(path).entries(), ["kept"]);
}

/// The file keeps the newest 2000, cut back once it is 200 past them; two
/// chats appending to one file both keep theirs.
#[test]
fn the_file_is_cut_to_the_newest_entries() {
    let dir = ScratchDir::new("history").unwrap();
    let path = dir.path().join("history");
    let mut a = History::load(path.clone());
    let mut b = History::load(path.clone());
    for i in 0..(MAX_ENTRIES + SLACK) {
        a.record(&format!("a{i}")).unwrap();
    }
    b.record("from b").unwrap();
    let entries = History::load(path.clone()).entries().to_vec();
    assert_eq!(entries.len(), MAX_ENTRIES);
    assert_eq!(entries.last().map(String::as_str), Some("from b"));
    assert_eq!(entries[0], format!("a{}", SLACK + 1));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap().lines().count(),
        MAX_ENTRIES
    );
}

#[test]
fn up_and_down_browse_from_the_newest() {
    let mut history = loaded(&["one", "two", "three"]);
    assert_eq!(history.older().as_deref(), Some("three"));
    assert_eq!(history.older().as_deref(), Some("two"));
    assert_eq!(history.older().as_deref(), Some("one"));
    assert_eq!(history.older(), None, "at the oldest");
    assert_eq!(history.newer().as_deref(), Some("two"));
    assert_eq!(history.newer().as_deref(), Some("three"));
    assert_eq!(
        history.newer().as_deref(),
        Some(""),
        "past the newest: empty"
    );
    assert_eq!(history.newer(), None);
}

/// Codex's rule: browse from an empty box, or from the entry just
/// recalled with the cursor at an end; a draft or an edit keeps Up and
/// Down for the cursor.
#[test]
fn browsing_starts_only_from_an_empty_box_or_a_recalled_entry() {
    let mut history = loaded(&["one", "two"]);
    assert!(history.browses("", 0));
    assert!(!history.browses("draft", 5));
    let shown = history.older().unwrap();
    assert!(history.browses(&shown, 0));
    assert!(history.browses(&shown, shown.len()));
    assert!(!history.browses(&shown, 1));
    assert!(!history.browses("two!", 4));
    assert!(!History::default().browses("", 0));
}

/// rp's file as GNU readline writes it: one entry per line, so a pasted
/// two-line message is two entries, as rp recalls it.
#[test]
fn readline_entries_are_rps_lines_without_blanks_or_repeats() {
    let rp = b"first\nls -la\nls -la\n\n   \ntwo\nlines\n#1762900000\nna\xefve \xff\nlast";
    assert_eq!(
        readline_entries(rp),
        [
            "first",
            "ls -la",
            "two",
            "lines",
            "#1762900000",
            "na\u{fffd}ve \u{fffd}",
            "last"
        ]
    );
    assert!(readline_entries(b"").is_empty());
}

#[test]
fn import_puts_rps_entries_before_viys_and_keeps_the_newest() {
    let dir = ScratchDir::new("history").unwrap();
    let path = dir.path().join(".viy").join("history");
    let mut history = History::load(path.clone());
    history.record("viy one").unwrap();
    history.record("viy\ntwo").unwrap();
    let rp: Vec<String> = ["rp one", "rp two"].map(String::from).to_vec();
    assert_eq!(import(&path, &rp, 1_700_000_000).unwrap(), 2);
    assert_eq!(
        History::load(path.clone()).entries(),
        ["rp one", "rp two", "viy one", "viy\ntwo"]
    );
    let first: Value = serde_json::from_str(
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        first,
        serde_json::json!({"ts": 1_700_000_000u64, "text": "rp one"})
    );
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // Past MAX_ENTRIES the oldest of rp's go first.
    let many: Vec<String> = (0..MAX_ENTRIES).map(|i| format!("old {i}")).collect();
    assert_eq!(import(&path, &many, 1).unwrap(), MAX_ENTRIES - 4);
    let entries = History::load(path).entries().to_vec();
    assert_eq!(entries.len(), MAX_ENTRIES);
    assert_eq!(entries[0], "old 4");
    assert_eq!(
        entries[MAX_ENTRIES - 4..],
        ["rp one", "rp two", "viy one", "viy\ntwo"]
    );
}

#[test]
fn import_into_no_history_makes_one() {
    let dir = ScratchDir::new("history").unwrap();
    let path = dir.path().join(".viy").join("history");
    assert_eq!(import(&path, &["a".to_string()], 5).unwrap(), 1);
    assert_eq!(History::load(path).entries(), ["a"]);
}

#[test]
fn import_twice_imports_once_and_a_repeat_at_the_seam_goes() {
    let dir = ScratchDir::new("history").unwrap();
    let path = dir.path().join(".viy").join("history");
    let mut history = History::load(path.clone());
    history.record("same").unwrap();
    history.record("viy").unwrap();
    let rp: Vec<String> = ["rp", "same"].map(String::from).to_vec();
    assert_eq!(import(&path, &rp, 7).unwrap(), 1);
    assert_eq!(History::load(path.clone()).entries(), ["rp", "same", "viy"]);
    assert_eq!(import(&path, &rp, 7).unwrap(), 0);
    assert_eq!(History::load(path).entries(), ["rp", "same", "viy"]);

    // After a cut took the oldest of rp's, still once.
    let many: Vec<String> = (0..MAX_ENTRIES + 5).map(|i| format!("old {i}")).collect();
    let path = dir.path().join("cut").join("history");
    assert_eq!(import(&path, &many, 1).unwrap(), MAX_ENTRIES);
    assert_eq!(import(&path, &many, 1).unwrap(), 0);
    assert_eq!(History::load(path).entries()[0], "old 5");
    let names: Vec<String> = std::fs::read_dir(dir.path().join(".viy"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["history"]);
}
