use std::fs;
use std::thread;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn scratch() -> (ScratchDir, Paths) {
    let dir = ScratchDir::new("pod-state").unwrap();
    let paths = Paths::under(dir.path());
    (dir, paths)
}

#[test]
fn no_file_unreadable_json_or_no_object_is_an_empty_state() {
    let (_dir, paths) = scratch();
    assert_eq!(load(&paths).unwrap(), PodState::default());
    fs::create_dir_all(paths.pod_state_dir()).unwrap();
    for text in ["", "{\"pod_id\": ", "[1, 2]", "null", "\u{ff}\u{fe}"] {
        fs::write(paths.pod_state_file(), text).unwrap();
        assert_eq!(load(&paths).unwrap(), PodState::default(), "{text:?}");
    }
}

#[test]
fn a_save_is_written_as_rp_writes_it() {
    let (_dir, paths) = scratch();
    save(
        &paths,
        [
            (key::POD_ID, json!("abc123")),
            (key::CREATED_AT, json!(1761650000.25)),
            (key::COST_PER_HR, json!(0.69)),
            (key::TUNNEL_ONLY, json!(true)),
        ],
    )
    .unwrap();
    save(&paths, [(key::TUNNEL_PID, Value::Null)]).unwrap();
    assert_eq!(
        fs::read_to_string(paths.pod_state_file()).unwrap(),
        "{\n  \"pod_id\": \"abc123\",\n  \"created_at\": 1761650000.25,\n  \
         \"cost_per_hr\": 0.69,\n  \"tunnel_only\": true,\n  \"tunnel_pid\": null\n}\n"
    );
}

#[test]
fn a_save_keeps_keys_it_does_not_set_in_their_place() {
    let (_dir, paths) = scratch();
    fs::create_dir_all(paths.pod_state_dir()).unwrap();
    fs::write(
        paths.pod_state_file(),
        r#"{"zeta": [1], "pod_id": "old", "alpha": {"x": null}}"#,
    )
    .unwrap();
    let after = save(&paths, [(key::POD_ID, json!("new")), ("beta", json!(2))]).unwrap();
    let keys: Vec<&str> = after.as_map().keys().map(String::as_str).collect();
    assert_eq!(keys, ["zeta", "pod_id", "alpha", "beta"]);
    assert_eq!(load(&paths).unwrap(), after);
    assert_eq!(after.get("alpha"), Some(&json!({"x": null})));
}

#[test]
fn clear_removes_the_file_and_is_quiet_without_one() {
    let (_dir, paths) = scratch();
    clear(&paths).unwrap();
    save(&paths, [(key::POD_ID, json!("p"))]).unwrap();
    clear(&paths).unwrap();
    assert!(!paths.pod_state_file().exists());
    assert!(paths.pod_state_lock_file().exists());
}

#[test]
fn the_getters_read_rps_keys() {
    let state = |v: Value| PodState(v.as_object().unwrap().clone());
    let s = state(json!({"pod_id": "p1", "volume_id": "v1", "tunnel_only": false}));
    assert_eq!(s.pod_id(), Some("p1"));
    assert_eq!(s.volume_id(), Some("v1"));
    assert_eq!(s.tunnel_only(), Some(false));
    let empty = state(json!({"pod_id": "", "volume_id": null}));
    assert_eq!(empty.pod_id(), None);
    assert_eq!(empty.volume_id(), None);
    assert_eq!(empty.tunnel_only(), None);
    for (v, only) in [
        (json!(true), true),
        (json!(null), false),
        (json!(1), true),
        (json!("no"), true),
        (json!([0]), true),
        (json!(0), false),
        (json!(-0.0), false),
        (json!(""), false),
        (json!([]), false),
        (json!({}), false),
    ] {
        assert_eq!(
            state(json!({"tunnel_only": v})).tunnel_only(),
            Some(only),
            "{v}"
        );
    }
}

#[test]
fn saves_from_many_writers_lose_nothing() {
    let (_dir, paths) = scratch();
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let paths = paths.clone();
            thread::spawn(move || {
                for i in 0..25 {
                    let k = format!("w{w}-{i}");
                    save(&paths, [(k.as_str(), json!(i))]).unwrap();
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    assert_eq!(load(&paths).unwrap().as_map().len(), 100);
}

#[test]
fn text_beyond_ascii_is_written_as_utf8_and_reads_back_the_same() {
    let (_dir, paths) = scratch();
    save(&paths, [(key::NAME, json!("qwen-ü"))]).unwrap();
    assert_eq!(
        fs::read_to_string(paths.pod_state_file()).unwrap(),
        "{\n  \"name\": \"qwen-ü\"\n}\n"
    );
    fs::write(paths.pod_state_file(), "{\"name\": \"qwen-\\u00fc\"}\n").unwrap();
    assert_eq!(load(&paths).unwrap().get(key::NAME), Some(&json!("qwen-ü")));
}

#[test]
fn an_unreadable_file_is_an_error_that_names_it() {
    let (_dir, paths) = scratch();
    fs::create_dir_all(paths.pod_state_file()).unwrap();
    let err = load(&paths).unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("{}: ", paths.pod_state_file().display())),
        "{err}"
    );
}
