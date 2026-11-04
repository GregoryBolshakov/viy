use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use pretty_assertions::assert_eq;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::Asking;
use viy_protocol::NoticeEvent;
use viy_protocol::Question;
use viy_testkit::scratch::ScratchDir;
use viy_tools::mounts::Mount;

use super::*;

/// A user who answers every question `answer`, and what was said and asked.
struct User {
    answer: Answer,
    said: Mutex<Vec<String>>,
    asked: Mutex<Vec<Question>>,
}

impl User {
    fn answering(answer: Answer) -> Self {
        Self {
            answer,
            said: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
        }
    }
}

impl Ui for User {
    fn notice(&self, notice: NoticeEvent) {
        self.said.lock().unwrap().push(notice.message);
    }

    fn ask(&self, question: &Question) -> Asking {
        self.asked.lock().unwrap().push(question.clone());
        let a = self.answer;
        Box::pin(async move { a })
    }
}

struct Fx {
    _scratch: ScratchDir,
    root: PathBuf,
    ws: PathBuf,
    tools: FileTools,
}

fn fx() -> Fx {
    let scratch = ScratchDir::new("files").unwrap();
    let root = scratch.path().to_path_buf();
    let ws = root.join("proj");
    std::fs::create_dir_all(ws.join(".rpctl")).unwrap();
    let tools = FileTools::new(
        ws.clone(),
        ws.clone(),
        root.join("state/locks"),
        root.join("state/quarantine"),
    );
    Fx {
        _scratch: scratch,
        root,
        ws,
        tools,
    }
}

fn args(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

impl Fx {
    async fn w(&mut self, path: &str, content: &str) -> Value {
        let user = User::answering(Answer::No);
        self.tools
            .write_file(
                &args(json!({"path": path, "content": content})),
                &user,
                "STAMP",
            )
            .await
            .result
    }

    async fn r(&mut self, a: Value) -> Value {
        let user = User::answering(Answer::No);
        self.tools.read_file(&args(a), &user).await.result
    }
}

// rpctl tests/test_workflows.py WriteProtectionTests, ported.

#[tokio::test]
async fn overwrite_needs_a_read_and_a_fresh_one() {
    let mut f = fx();
    std::fs::write(f.ws.join("a.txt"), "one").unwrap();
    assert!(
        f.w("a.txt", "two").await["error"]
            .as_str()
            .unwrap()
            .contains("have not read it")
    );
    f.r(json!({"path": "a.txt"})).await;
    assert_eq!(f.w("a.txt", "two").await["ok"], true);
    assert_eq!(f.w("a.txt", "three").await["ok"], true); // own write counts as seen
    std::fs::write(f.ws.join("a.txt"), "someone else").unwrap();
    assert!(
        f.w("a.txt", "four").await["error"]
            .as_str()
            .unwrap()
            .contains("changed on disk")
    );
    assert_eq!(
        std::fs::read_to_string(f.ws.join("a.txt")).unwrap(),
        "someone else"
    );
}

#[tokio::test]
async fn two_chats_cannot_lose_each_others_index_line() {
    let mut a = fx();
    let mut b = FileTools::new(
        a.ws.clone(),
        a.ws.clone(),
        a.root.join("state/locks"),
        a.root.join("state/quarantine"),
    );
    let user = User::answering(Answer::No);
    std::fs::write(a.ws.join("index.md"), "- f1\n").unwrap();
    a.r(json!({"path": "index.md"})).await;
    b.read_file(&args(json!({"path": "index.md"})), &user).await;
    assert_eq!(a.w("index.md", "- f1\n- f2\n").await["ok"], true);
    let got = b
        .write_file(
            &args(json!({"path": "index.md", "content": "- f1\n- f3\n"})),
            &user,
            "S",
        )
        .await
        .result;
    assert!(got["error"].as_str().unwrap().contains("changed on disk"));
    b.read_file(&args(json!({"path": "index.md"})), &user).await;
    let got = b
        .write_file(
            &args(json!({"path": "index.md", "content": "- f1\n- f2\n- f3\n"})),
            &user,
            "S",
        )
        .await
        .result;
    assert_eq!(got["ok"], true);
}

#[tokio::test]
async fn kb_fact_id_collision_names_the_next_free_id() {
    let mut f = fx();
    let facts = f.ws.join("kb/facts");
    std::fs::create_dir_all(&facts).unwrap();
    std::fs::write(facts.join("f041-alpha.md"), "x").unwrap();
    std::fs::write(facts.join("f042-beta.md"), "x").unwrap();
    let err = f.w("kb/facts/f041-gamma.md", "x").await;
    assert!(err["error"].as_str().unwrap().contains("f043"), "{err}");
    assert_eq!(f.w("kb/facts/f043-gamma.md", "x").await["ok"], true);
    assert_eq!(f.w("notes/f041-gamma.md", "x").await["ok"], true); // not a KB dir
}

#[tokio::test]
async fn write_requires_read_can_be_turned_off() {
    let mut f = fx();
    f.tools.write_requires_read = false;
    std::fs::write(f.ws.join("a.txt"), "one").unwrap();
    assert_eq!(f.w("a.txt", "two").await["ok"], true);
}

// rpctl tests/test_workflows.py ReadRangeTests, ported.

#[tokio::test]
async fn line_ranges_in_both_spellings() {
    let mut f = fx();
    let body: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    std::fs::write(f.ws.join("t.txt"), &body).unwrap();
    let r = f
        .r(json!({"path": "t.txt", "start_line": 3, "end_line": 4}))
        .await;
    assert_eq!(r["content"], "3: line 3\n4: line 4");
    assert_eq!(r["lines"], "3-4 of 11");
    let r = f.r(json!({"path": "t.txt", "offset": 9, "limit": 5})).await;
    assert_eq!(r["content"], "9: line 9\n10: line 10\n11: ");
    // The keys in rp's order.
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["path", "lines", "content", "truncated"]);
}

#[tokio::test]
async fn a_cut_file_says_how_to_read_the_rest() {
    let mut f = fx();
    f.tools.max_output = 50;
    let body: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    std::fs::write(f.ws.join("big.txt"), &body).unwrap();
    let r = f.r(json!({"path": "big.txt"})).await;
    assert_eq!(r["truncated"], true);
    assert!(r["content"].as_str().unwrap().contains("chars omitted"));
    assert!(r["note"].as_str().unwrap().contains("41 lines"));
    assert!(r["note"].as_str().unwrap().contains("start_line/end_line"));
}

// Confinement, guards, mounts (rpctl test_mounts ChatTests' file parts).

#[tokio::test]
async fn paths_stay_in_the_workspace() {
    let mut f = fx();
    std::fs::write(f.root.join("outside.txt"), "x").unwrap();
    let r = f.r(json!({"path": "../outside.txt"})).await;
    assert_eq!(r["error"], "path escapes the workspace");
    let abs = f.root.join("outside.txt");
    let r = f.r(json!({"path": abs.to_str().unwrap()})).await;
    assert_eq!(r["error"], "path must be relative to the workspace");
    std::os::unix::fs::symlink(&abs, f.ws.join("link")).unwrap();
    let r = f.r(json!({"path": "link"})).await;
    assert_eq!(r["error"], "path escapes the workspace");
    std::os::unix::fs::symlink("loop", f.ws.join("loop")).unwrap();
    let r = f.r(json!({"path": "loop"})).await;
    assert!(r["error"].as_str().unwrap().contains("symbolic links"));
    assert_eq!(f.r(json!({})).await["error"], "no path given");
}

#[tokio::test]
async fn protected_writes_and_secret_reads_ask() {
    let mut f = fx();
    let no = User::answering(Answer::No);
    let got = f
        .tools
        .write_file(
            &args(json!({"path": ".rpctl/config", "content": "x"})),
            &no,
            "S",
        )
        .await;
    assert!(got.refused);
    assert_eq!(got.result["declined"], true);
    assert!(
        got.result["note"]
            .as_str()
            .unwrap()
            .starts_with("the user declined to let you write this file: .rpctl runs")
    );
    let q = no.asked.lock().unwrap()[0].clone();
    assert_eq!(q.prompt, "  let the model write it?");
    assert_eq!(q.kind, Some(viy_protocol::QuestionKind::ProtectedWrite));
    assert_eq!(
        q.reason[0].message,
        "  [write] .rpctl/config is protected: .rpctl runs or configures code outside the sandbox"
    );
    assert_eq!(no.said.lock().unwrap().as_slice(), ["  [declined]"]);
    assert!(!f.ws.join(".rpctl/config").exists());

    std::fs::write(f.ws.join(".env"), "KEY=1\n").unwrap();
    let got = f.tools.read_file(&args(json!({"path": ".env"})), &no).await;
    assert!(got.refused);
    let yes = User::answering(Answer::Yes);
    let got = f
        .tools
        .read_file(&args(json!({"path": ".env"})), &yes)
        .await;
    assert_eq!(got.result["content"], "KEY=1\n");
    assert_eq!(
        yes.asked.lock().unwrap()[0].kind,
        Some(viy_protocol::QuestionKind::SecretRead)
    );
}

/// rpctl test_mounts ChatTests.test_read_only_mount_for_the_file_tools and
/// test_rw_mount_keeps_the_workspace_rules, the file tools' half (the
/// /mount command, the note and the taint come with the chat glue).
#[tokio::test]
async fn mounts_for_the_file_tools() {
    let mut f = fx();
    let drive = f.root.join("drive");
    std::fs::create_dir_all(drive.join("docs")).unwrap();
    std::fs::write(drive.join("docs/a.txt"), "hello from the drive\n").unwrap();
    let file = drive.join("docs/a.txt");
    let file = file.to_str().unwrap();
    let r = f.r(json!({"path": file})).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("relative to the workspace")
    );
    f.tools.mounts = vec![Mount::new(&drive, false)];
    assert_eq!(
        f.r(json!({"path": file})).await["content"],
        "hello from the drive\n"
    );
    let listed = f
        .tools
        .list_dir(&args(json!({"path": drive.to_str().unwrap()})))
        .result;
    assert!(
        listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "docs/")
    );
    let new = drive.join("new.txt");
    let r = f.w(new.to_str().unwrap(), "x").await;
    assert!(r["error"].as_str().unwrap().contains("mounted read-only"));
    assert!(!new.exists());
    let outside = f.root.join("proj/../x");
    let r = f.r(json!({"path": outside.to_str().unwrap()})).await;
    assert!(r["error"].as_str().unwrap().contains("mounted folder"));

    std::fs::create_dir(drive.join(".git")).unwrap();
    f.tools.mounts = vec![Mount::new(&drive, true)];
    assert_eq!(f.w(new.to_str().unwrap(), "x").await["ok"], true);
    let r = f
        .w(drive.join(".git/config").to_str().unwrap(), "[core]\n")
        .await;
    assert_eq!(r["declined"], true);
}

#[tokio::test]
async fn list_dir_names_folders_and_sizes() {
    let f = fx();
    std::fs::write(f.ws.join("b.txt"), "12345").unwrap();
    std::fs::create_dir(f.ws.join("a")).unwrap();
    let r = f.tools.list_dir(&args(json!({}))).result;
    assert_eq!(r["path"], ".");
    assert_eq!(
        r["entries"],
        json!([
            {"name": ".rpctl/", "type": "dir", "bytes": null},
            {"name": "a/", "type": "dir", "bytes": null},
            {"name": "b.txt", "type": "file", "bytes": 5},
        ])
    );
}

#[tokio::test]
async fn a_bare_repository_written_file_by_file_is_quarantined() {
    let mut f = fx();
    f.tools.write_requires_read = false;
    std::fs::create_dir_all(f.ws.join("evil/objects")).unwrap();
    std::fs::create_dir_all(f.ws.join("evil/refs")).unwrap();
    let r = f.w("evil/HEAD", "ref: refs/heads/main\n").await;
    assert_eq!(r["quarantined"], json!(["evil"]));
    assert!(!f.ws.join("evil").exists());
    assert!(f.root.join("state/quarantine/STAMP/evil/HEAD").is_file());
}

#[test]
fn line_span_edges() {
    let a = |v: Value| args(v);
    assert_eq!(line_span(&a(json!({})), 5), None);
    assert_eq!(line_span(&a(json!({"start_line": 0})), 5), Some((1, 5)));
    assert_eq!(line_span(&a(json!({"start_line": 9})), 5), Some((5, 5)));
    assert_eq!(line_span(&a(json!({"end_line": -2})), 5), Some((1, 1)));
    assert_eq!(
        line_span(&a(json!({"start_line": "2", "end_line": "3"})), 5),
        Some((2, 3))
    );
    assert_eq!(line_span(&a(json!({"limit": 2})), 5), Some((1, 2)));
    let _ = Path::new("");
}

// Found by the WP 4.5 review.

#[tokio::test]
async fn a_path_that_is_not_a_string_is_refused() {
    let mut f = fx();
    let user = User::answering(Answer::No);
    for (path, want) in [
        (json!(5), "path must be a string"),
        (json!({"a": 1}), "path must be a string"),
        (json!(true), "path must be a string"),
        (json!(0), "no path given"),
        (json!([]), "no path given"),
        (json!(false), "no path given"),
    ] {
        let got = f
            .tools
            .write_file(&args(json!({"path": path, "content": "x"})), &user, "S")
            .await;
        assert_eq!(got.result["error"], want, "{path}");
    }
    assert_eq!(std::fs::read_dir(&f.ws).unwrap().count(), 1, "only .rpctl");
    assert_eq!(
        f.tools.list_dir(&args(json!({"path": 0}))).result["path"],
        "."
    );
    assert_eq!(
        f.tools.list_dir(&args(json!({"path": 7}))).result["error"],
        "path must be a string"
    );
}

#[tokio::test]
async fn a_huge_range_reads_to_the_end() {
    let mut f = fx();
    std::fs::write(f.ws.join("t.txt"), "a\nb\nc").unwrap();
    let r = f
        .r(json!({"path": "t.txt", "offset": 2, "limit": i64::MAX}))
        .await;
    assert_eq!(r["content"], "2: b\n3: c");
    let r = f
        .r(json!({"path": "t.txt", "offset": 2, "limit": 1e300}))
        .await;
    assert_eq!(r["content"], "2: b\n3: c");
}

#[tokio::test]
async fn a_failed_quarantine_is_an_error_for_the_model() {
    let mut f = fx();
    f.tools.write_requires_read = false;
    std::fs::create_dir_all(f.ws.join("evil/objects")).unwrap();
    std::fs::create_dir_all(f.ws.join("evil/refs")).unwrap();
    // The quarantine folder cannot be made: a file stands in its way.
    std::fs::create_dir_all(f.root.join("state")).unwrap();
    std::fs::write(f.root.join("state/quarantine"), "").unwrap();
    let r = f.w("evil/HEAD", "ref: refs/heads/main\n").await;
    let err = r["error"].as_str().unwrap();
    assert!(
        err.starts_with("wrote evil/HEAD, but could not move the bare repository evil"),
        "{err}"
    );
}

#[tokio::test]
async fn kb_ids_as_rpctl_reads_them() {
    let mut f = fx();
    let facts = f.ws.join("kb/facts");
    std::fs::create_dir_all(&facts).unwrap();
    std::fs::write(facts.join("f0001-a.md"), "x").unwrap();
    // Python's `$` matches before a final newline.
    let r = f.w("kb/facts/f0001-b.md\n", "x").await;
    assert!(r["error"].as_str().unwrap().contains("f0002"), "{r}");
    // Digits Python reads and viy cannot compare: refused.
    let r = f.w("kb/facts/f\u{661}-b.md", "x").await;
    assert!(r["error"].as_str().unwrap().contains("digits 0-9"), "{r}");
    // Numbers of any length.
    let big = "9".repeat(45);
    std::fs::write(facts.join(format!("f{big}-c.md")), "x").unwrap();
    let r = f.w(&format!("kb/facts/f{big}-d.md"), "x").await;
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains(&format!("f1{}", "0".repeat(45))),
        "{r}"
    );
}
