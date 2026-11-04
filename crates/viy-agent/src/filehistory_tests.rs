use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn sha(text: &str) -> String {
    sha1_hex(text.as_bytes())
}

struct Fx {
    _scratch: ScratchDir,
    root: PathBuf,
}

fn fx() -> Fx {
    let scratch = ScratchDir::new("filehistory").unwrap();
    let root = viy_tools::resolve::resolve(scratch.path());
    Fx {
        _scratch: scratch,
        root,
    }
}

/// Both kinds of record, for tests that hold for each.
fn histories(fx: &Fx) -> [FileHistory; 2] {
    [
        FileHistory::in_memory(),
        FileHistory::at(fx.root.join("s.files")),
    ]
}

#[test]
fn records_rows_and_blobs_and_plans_per_file() {
    let fx = fx();
    let (a, b) = (fx.root.join("a.txt"), fx.root.join("b.txt"));
    for mut h in histories(&fx) {
        h.record("w1", &a, Some(b"v1"), &sha("v2")).unwrap();
        h.record("w2", &b, None, &sha("new")).unwrap();
        h.record("w3", &a, Some(b"v2"), &sha("v3")).unwrap();
        h.record("", &a, Some(b"v3"), &sha("v4")).unwrap();
        let rows = h.rows();
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["w1", "w2", "w3"],
            "a call without an id is not recorded"
        );
        assert_eq!(rows[1].before, None);
        assert_eq!(h.blob(&sha("v1")).as_deref(), Some(&b"v1"[..]));
        assert_eq!(h.blob(&sha("nothing")), None);
        assert_eq!(
            h.plan(["w2", "w3", "w1"]),
            [
                Planned {
                    path: a.clone(),
                    before: Some(sha("v1")),
                    after: sha("v3"),
                },
                Planned {
                    path: b.clone(),
                    before: None,
                    after: sha("new"),
                },
            ],
            "in the order first written: before from the first, after from the last"
        );
        assert_eq!(h.plan(["w3"])[0].before, Some(sha("v2")));
        assert_eq!(h.plan(["other"]), []);
    }
}

#[test]
fn the_folder_holds_rps_layout() {
    let fx = fx();
    let folder = fx.root.join("s.files");
    let mut h = FileHistory::at(folder.clone());
    h.record("w1", &fx.root.join("a.txt"), Some(b"v1"), &sha("v2"))
        .unwrap();
    h.record("w2", &fx.root.join("a.txt"), Some(b"v1"), &sha("v2"))
        .unwrap();
    let mut names: Vec<String> = fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [sha("v1"), "writes.jsonl".into()],
        "a blob is kept once"
    );
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&folder), 0o700);
    assert_eq!(mode(&folder.join("writes.jsonl")), 0o600);
    assert_eq!(mode(&folder.join(sha("v1"))), 0o600);
    // rp's lines, as rp's json.loads reads them.
    let text = fs::read_to_string(folder.join("writes.jsonl")).unwrap();
    let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["id"], "w1");
    assert_eq!(first["path"], fx.root.join("a.txt").to_str().unwrap());
    assert_eq!(first["before"], sha("v1"));
    assert_eq!(first["after"], sha("v2"));
    assert!(first["ts"].as_f64().unwrap() > 1.7e9);
}

#[test]
fn rows_rp_wrote_are_read_and_damage_is_skipped() {
    let fx = fx();
    let folder = fx.root.join("s.files");
    fs::create_dir(&folder).unwrap();
    fs::write(
        folder.join("writes.jsonl"),
        concat!(
            "{\"id\": \"w1\", \"path\": \"/w/a.txt\", \"before\": null, \"after\": \"ab\", \"ts\": 1.5}\n",
            "not json\n",
            "{\"id\": \"\", \"path\": \"/w/a.txt\", \"before\": null, \"after\": \"ab\"}\n",
            "{\"id\": 7, \"path\": \"/w/a.txt\", \"before\": null, \"after\": \"ab\"}\n",
            "{\"id\": \"w2\", \"path\": \"/w/b.txt\", \"before\": \"cd\", \"after\": \"ef\", \"ts\": 2}\n",
            "{\"id\": \"w3\", \"path\": \"/w/c.txt\", \"bef",
        ),
    )
    .unwrap();
    let h = FileHistory::at(folder);
    let ids: Vec<String> = h.rows().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["w1", "w2"], "a line cut short ends the list");
    assert_eq!(FileHistory::at(fx.root.join("none")).rows(), []);
}

#[test]
fn a_blob_name_must_be_a_digest() {
    let fx = fx();
    let folder = fx.root.join("s.files");
    fs::create_dir(&folder).unwrap();
    fs::write(fx.root.join("secret"), "x").unwrap();
    let h = FileHistory::at(folder);
    assert_eq!(h.blob("../secret"), None);
    assert_eq!(h.blob("writes.jsonl"), None);
}

fn planned(path: &Path, before: Option<&str>, after: &str) -> Planned {
    Planned {
        path: path.to_path_buf(),
        before: before.map(sha),
        after: sha(after),
    }
}

#[test]
fn actions_say_what_undoing_takes_and_apply_does_it() {
    let fx = fx();
    let p = |name: &str| fx.root.join(name);
    for name in ["restore", "same", "changed", "lost"] {
        fs::write(p(name), "now").unwrap();
    }
    fs::write(p("delete"), "made").unwrap();
    fs::write(p("same"), "old").unwrap();
    let mut h = FileHistory::in_memory();
    h.record("w", &p("restore"), Some(b"old"), &sha("now"))
        .unwrap();
    let files = [
        planned(&p("restore"), Some("old"), "now"),
        planned(&p("delete"), None, "made"),
        planned(&p("same"), Some("old"), "now"),
        planned(&p("changed"), Some("old"), "other"),
        planned(&p("lost"), Some("never kept"), "now"),
        planned(&p("gone"), None, "made"),
    ];
    let want = [
        Action::Restore,
        Action::Delete,
        Action::Same,
        Action::Changed,
        Action::Lost,
        Action::Same,
    ];
    assert_eq!(h.actions(&files, false), want);
    assert_eq!(fs::read_to_string(p("restore")).unwrap(), "now");
    assert_eq!(h.actions(&files, true), want);
    assert_eq!(fs::read_to_string(p("restore")).unwrap(), "old");
    assert!(!p("delete").exists());
    assert_eq!(fs::read_to_string(p("changed")).unwrap(), "now");
    assert_eq!(fs::read_to_string(p("lost")).unwrap(), "now");
}

#[test]
fn a_blob_that_does_not_match_its_name_is_lost() {
    let fx = fx();
    let folder = fx.root.join("s.files");
    let path = fx.root.join("a.txt");
    fs::write(&path, "now").unwrap();
    let mut h = FileHistory::at(folder.clone());
    h.record("w", &path, Some(b"old"), &sha("now")).unwrap();
    fs::write(folder.join(sha("old")), "tampered").unwrap();
    let files = h.plan(["w"]);
    assert_eq!(h.actions(&files, true), [Action::Lost]);
    assert_eq!(fs::read_to_string(&path).unwrap(), "now");
}

#[test]
fn undoing_never_goes_through_a_link_made_since() {
    let fx = fx();
    let ws = fx.root.join("ws");
    let outside = fx.root.join("outside");
    fs::create_dir_all(ws.join("sub")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    // The model wrote ws/a.txt and ws/sub/b.txt, then put links there that
    // lead out of the workspace to files holding what it wrote.
    fs::write(outside.join("a.txt"), "now").unwrap();
    fs::write(outside.join("b.txt"), "made").unwrap();
    std::os::unix::fs::symlink(outside.join("a.txt"), ws.join("a.txt")).unwrap();
    fs::remove_dir(ws.join("sub")).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("sub")).unwrap();
    let mut h = FileHistory::in_memory();
    h.record("w1", &ws.join("a.txt"), Some(b"old"), &sha("now"))
        .unwrap();
    h.record("w2", &ws.join("sub/b.txt"), None, &sha("made"))
        .unwrap();
    let files = h.plan(["w1", "w2"]);
    assert_eq!(h.actions(&files, true), [Action::Changed, Action::Changed]);
    assert_eq!(fs::read_to_string(outside.join("a.txt")).unwrap(), "now");
    assert!(outside.join("b.txt").exists());
}

#[test]
fn a_failed_undo_says_why() {
    let fx = fx();
    let dir = fx.root.join("ro");
    fs::create_dir(&dir).unwrap();
    let path = dir.join("a.txt");
    fs::write(&path, "made").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let h = FileHistory::in_memory();
    let got = h.actions(&[planned(&path, None, "made")], true);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    match &got[..] {
        [Action::Failed(why)] => assert!(why.contains("ermission denied"), "{why}"),
        other => panic!("{other:?}"),
    }
}
