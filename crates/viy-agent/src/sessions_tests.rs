//! rpctl's SessionLogTests, the session halves of RedactTests and
//! TitleTests, and the `chat/sessions` fixture: every script of steps run
//! again through viy's sessions must leave the events, replay, sidecars,
//! listings, searches and resolves rpctl's left, and viy must read each of
//! rpctl's logs, byte for byte as rp wrote them, as rp reads it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::PoisonError;

use pretty_assertions::assert_eq;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// A clock that reads what it is set to.
#[derive(Debug, Default)]
struct StepClock(Mutex<f64>);

impl StepClock {
    fn set(&self, t: f64) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = t;
    }
}

impl TimeProvider for StepClock {
    fn now(&self) -> DateTime<Utc> {
        let t = *self.0.lock().unwrap_or_else(PoisonError::into_inner);
        DateTime::from_timestamp(t.floor() as i64, ((t - t.floor()) * 1e9) as u32).unwrap()
    }
}

struct Store {
    _scratch: ScratchDir,
    root: PathBuf,
    clock: Arc<StepClock>,
    sessions: Sessions,
}

fn store() -> Store {
    let scratch = ScratchDir::new("sessions").unwrap();
    let root = scratch.path().to_path_buf();
    let clock = Arc::new(StepClock::default());
    clock.set(1761868800.25);
    let sessions = Sessions::new(root.join("sessions"), clock.clone());
    Store {
        _scratch: scratch,
        root,
        clock,
        sessions,
    }
}

impl Store {
    fn project(&self) -> String {
        self.root.join("proj").display().to_string()
    }

    fn new_log(&self) -> SessionLog {
        self.sessions.create(Some(&self.project()), None)
    }

    fn files(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = fs::read_dir(self.sessions.dir())
            .into_iter()
            .flatten()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        out.sort();
        out
    }
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": text})
}

fn kinds(path: &Path) -> Vec<String> {
    read_events(path)
        .unwrap()
        .iter()
        .map(|e| e["t"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn nothing_written_until_first_message() {
    let s = store();
    let mut log = s.new_log();
    assert!(!log.sync(Some("sys"), &[], false).unwrap());
    log.close("exit").unwrap();
    assert_eq!(s.files(), Vec::<PathBuf>::new());
}

#[test]
fn append_and_replay_roundtrip() {
    let s = store();
    let mut log = s.sessions.create(Some(&s.project()), Some("m"));
    let mut msgs = vec![user("hello there")];
    log.sync(Some("be brief"), &msgs, false).unwrap();
    msgs.push(assistant("hi"));
    log.sync(Some("be brief"), &msgs, false).unwrap();
    log.close("exit").unwrap();
    let st = replay(log.path()).unwrap();
    assert_eq!(st.messages, msgs);
    assert_eq!(st.system.as_deref(), Some("be brief"));
    assert_eq!(st.title.as_deref(), Some("hello there"));
    assert_eq!(
        kinds(log.path()),
        ["meta", "system", "msg", "title", "msg", "end"]
    );
    let mode = fs::metadata(log.path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn rewrite_is_a_replace_and_keeps_the_old_messages() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("start"), assistant("the zebra detail")], false)
        .unwrap();
    let summary = [
        user("[Summary of the conversation so far.]"),
        assistant("Understood."),
    ];
    log.sync(None, &summary, false).unwrap();
    log.close("exit").unwrap();
    assert_eq!(replay(log.path()).unwrap().messages, summary);
    let hits = s.sessions.search("zebra", None);
    assert_eq!(
        hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
        [log.id()]
    );
    assert!(hits[0].snippet.as_deref().unwrap().contains("zebra"));
}

#[test]
fn torn_last_line_is_ignored_and_trimmed_on_resume() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("a")], false).unwrap();
    log.close("exit").unwrap();
    let mut file = OpenOptions::new().append(true).open(log.path()).unwrap();
    file.write_all(br#"{"t": "msg", "message": {"role": "assi"#)
        .unwrap();
    assert_eq!(replay(log.path()).unwrap().messages.len(), 1);
    let (mut again, st) = s.sessions.open(log.id()).unwrap();
    let mut msgs = st.messages;
    msgs.push(assistant("b"));
    again.sync(None, &msgs, false).unwrap();
    again.close("exit").unwrap();
    let contents: Vec<Value> = replay(log.path())
        .unwrap()
        .messages
        .iter()
        .map(|m| m["content"].clone())
        .collect();
    assert_eq!(contents, [json!("a"), json!("b")]);
    for line in fs::read(log.path()).unwrap().split(|&b| b == b'\n') {
        if !line.is_empty() {
            serde_json::from_slice::<Value>(line).unwrap();
        }
    }
}

#[test]
fn user_title_beats_automatic_title() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user(&"x".repeat(100))], false).unwrap();
    assert!(log.title().unwrap().ends_with('…'));
    assert!(log.title().unwrap().chars().count() <= TITLE_CHARS);
    log.rename("My plan").unwrap();
    log.append(json!({"t": "title", "text": "auto later", "auto": true}))
        .unwrap();
    log.close("exit").unwrap();
    assert_eq!(
        replay(log.path()).unwrap().title.as_deref(),
        Some("My plan")
    );
    assert_eq!(s.sessions.list(None)[0].title, "My plan");
}

#[test]
fn second_writer_is_refused() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("a")], false).unwrap();
    assert!(matches!(
        s.sessions.open(log.id()),
        Err(SessionError::Busy(_))
    ));
    assert!(matches!(
        s.sessions.archive(log.id()),
        Err(SessionError::Busy(_))
    ));
    assert!(matches!(
        s.sessions.delete(log.id()),
        Err(SessionError::Busy(_))
    ));
    log.close("exit").unwrap();
    let (mut other, _) = s.sessions.open(log.id()).unwrap();
    other.close("exit").unwrap();
}

#[test]
fn fork_copies_history_into_a_new_session() {
    let s = store();
    let mut log = s.new_log();
    let msgs = [user("a")];
    log.sync(None, &msgs, false).unwrap();
    let mut twin = log.fork(None, &msgs, None).unwrap();
    log.close("exit").unwrap();
    twin.close("exit").unwrap();
    assert_ne!(twin.id(), log.id());
    let st = replay(twin.path()).unwrap();
    assert_eq!(st.messages, msgs);
    assert!(st.title.unwrap().ends_with("(fork)"));
    // A model title would name the shared start, so the fork gets none.
    assert!(twin.title_by_model());
}

#[test]
fn a_copy_of_a_held_chat_keeps_its_project_and_model() {
    let s = store();
    let mut log = s.sessions.create(Some(&s.project()), Some("m"));
    let msgs = [user("held"), assistant("elsewhere")];
    log.sync(Some("sys"), &msgs, false).unwrap();
    let (mut twin, st) = s.sessions.copy(log.id(), None, false).unwrap();
    twin.close("exit").unwrap();
    assert_eq!(st.messages, msgs);
    let copied = replay(twin.path()).unwrap();
    assert_eq!(copied.messages, msgs);
    assert_eq!(copied.system.as_deref(), Some("sys"));
    assert_eq!(copied.meta_str("project"), Some(s.project()));
    assert_eq!(copied.meta_str("model").as_deref(), Some("m"));
    assert_eq!(copied.title.as_deref(), Some("held (fork)"));
    // The copy lists with the project's chats, not only under `-a`.
    assert!(
        s.sessions
            .list(Some(&s.project()))
            .iter()
            .any(|r| r.id == twin.id())
    );
    // The original was only read: its writer goes on.
    log.sync(
        Some("sys"),
        &[user("held"), assistant("elsewhere"), user("more")],
        false,
    )
    .unwrap();
    log.close("exit").unwrap();
    assert_eq!(replay(log.path()).unwrap().messages.len(), 3);
}

#[test]
fn listing_is_project_scoped_and_title_hits_rank_first() {
    let s = store();
    let project = s.project();
    let mut ids = Vec::new();
    for (project, msgs) in [
        (project.as_str(), vec![user("talk about apples")]),
        (
            project.as_str(),
            vec![user("unrelated"), assistant("but apples appear here")],
        ),
        ("/elsewhere", vec![user("apples elsewhere")]),
    ] {
        s.clock.set(s.clock.now().timestamp() as f64 + 1.0);
        let mut log = s.sessions.create(Some(project), None);
        log.sync(None, &msgs, false).unwrap();
        log.close("exit").unwrap();
        ids.push(log.id().to_string());
    }
    let listed: std::collections::HashSet<String> = s
        .sessions
        .list(Some(&project))
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(listed, [ids[0].clone(), ids[1].clone()].into());
    let found: Vec<String> = s
        .sessions
        .search("apples", Some(&project))
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(found, [ids[0].clone(), ids[1].clone()]);
    assert_eq!(s.sessions.search("apples", None).len(), 3);
    assert_eq!(s.sessions.search("apples pears", None), []);
}

#[test]
fn scope_follows_the_project_tree_not_exact_path() {
    let s = store();
    let project = s.project();
    let parent = s.root.display().to_string();
    let sub = s.root.join("proj/sub").display().to_string();
    let sib = s.root.join("other").display().to_string();
    let mut made = BTreeMap::new();
    for (name, proj) in [
        ("self", &project),
        ("sub", &sub),
        ("parent", &parent),
        ("sib", &sib),
    ] {
        let mut log = s.sessions.create(Some(proj), None);
        log.sync(None, &[user(name)], false).unwrap();
        log.close("exit").unwrap();
        made.insert(name, log.id().to_string());
    }
    let seen = |scope: &str| -> std::collections::BTreeSet<String> {
        s.sessions
            .list(Some(scope))
            .into_iter()
            .map(|r| r.id)
            .collect()
    };
    let want: std::collections::BTreeSet<String> =
        ["self", "sub", "parent"].map(|n| made[n].clone()).into();
    // From proj/: itself, a chat filed in a sub-folder, one filed against a
    // parent, never a sibling's.
    assert_eq!(seen(&project), want);
    assert_eq!(seen(&sub), want);
    assert!(!seen(&project).contains(&made["sib"]));
}

#[test]
fn stale_sidecar_is_rebuilt() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("first")], false).unwrap();
    log.close("exit").unwrap();
    let side = sidecar(log.path());
    fs::remove_file(&side).unwrap();
    assert_eq!(s.sessions.list(None)[0].messages, 1);
    assert!(side.exists());
}

/// rp's `test_trash_moves_the_log`, as Codex's archive: the log leaves
/// the lists for `archived_sessions/`, and comes back.
#[test]
fn archive_moves_the_log_and_unarchive_brings_it_back() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("a")], false).unwrap();
    log.close("exit").unwrap();
    fs::create_dir(log.files_dir()).unwrap();
    s.sessions.archive(log.id()).unwrap();
    assert_eq!(s.sessions.list(None), []);
    let archived = s.sessions.archived();
    assert_eq!(archived.dir(), s.root.join("archived_sessions"));
    assert_eq!(archived.list(None)[0].id, log.id());
    assert!(files_dir(&archived.path_of(log.id())).is_dir());
    s.sessions.unarchive(log.id()).unwrap();
    assert_eq!(archived.list(None), []);
    assert_eq!(s.sessions.list(None)[0].id, log.id());
    assert!(log.files_dir().is_dir());
}

#[test]
fn delete_removes_the_log_and_its_files() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("a")], false).unwrap();
    log.close("exit").unwrap();
    fs::create_dir(log.files_dir()).unwrap();
    s.sessions.delete(log.id()).unwrap();
    assert_eq!(s.sessions.list(None), []);
    assert!(!log.path().exists());
    assert!(!sidecar(log.path()).exists());
    assert!(!log.files_dir().exists());
}

#[test]
fn lookup_takes_an_id_or_the_exact_name_of_one_interactive_chat() {
    let s = store();
    let mut a = s.new_log();
    a.sync(None, &[user("a")], false).unwrap();
    a.rename("Plan").unwrap();
    a.close("exit").unwrap();
    assert_eq!(s.sessions.lookup(a.id()).unwrap().id, a.id());
    assert_eq!(s.sessions.lookup("Plan").unwrap().id, a.id());
    assert_eq!(s.sessions.lookup("plan"), Err(LookupError::Missing));
    let mut b = s.new_log();
    b.mark_exec();
    b.sync(None, &[user("b")], false).unwrap();
    b.rename("Plan").unwrap();
    b.close("exec").unwrap();
    // A one-shot chat answers to its id only.
    assert_eq!(s.sessions.lookup("Plan").unwrap().id, a.id());
    assert_eq!(s.sessions.lookup(b.id()).unwrap().id, b.id());
    assert!(s.sessions.lookup(b.id()).unwrap().exec);
    let mut c = s.new_log();
    c.sync(None, &[user("c")], false).unwrap();
    c.rename("Plan").unwrap();
    c.close("exit").unwrap();
    assert!(matches!(
        s.sessions.lookup("Plan"),
        Err(LookupError::Ambiguous(_, _))
    ));
}

#[test]
fn session_ids_have_one_shape() {
    let s = store();
    let log = s.new_log();
    assert!(is_session_id(log.id()), "{}", log.id());
    for not in [
        "Plan",
        "20260718-135451-84e",
        "20260718-135451-84eg",
        "2026103a-135451-84e1",
    ] {
        assert!(!is_session_id(not), "{not}");
    }
}

#[test]
fn a_one_shot_chat_is_marked_in_its_log_and_sidecar() {
    let s = store();
    let mut log = s.new_log();
    log.mark_exec();
    log.sync(None, &[user("a")], false).unwrap();
    log.close("exec").unwrap();
    assert_eq!(read_events(log.path()).unwrap()[0]["exec"], true);
    let side: Value = serde_json::from_slice(&fs::read(sidecar(log.path())).unwrap()).unwrap();
    assert_eq!(side["exec"], true);
    let (reopened, _) = s.sessions.open(log.id()).unwrap();
    drop(reopened);
    fs::remove_file(sidecar(log.path())).unwrap();
    assert!(s.sessions.list(None)[0].exec);
}

#[test]
fn session_log_masks_on_disk_only() {
    let s = store();
    let tok = format!("hf_{}", "z".repeat(34));
    let mut log = s.new_log();
    log.set_redactor(Some(Redactor::default()));
    let msgs = [user(&format!("use {tok}"))];
    log.sync(Some("sys"), &msgs, false).unwrap();
    log.sync(Some("sys"), &[msgs[0].clone(), assistant("ok")], false)
        .unwrap();
    log.close("exit").unwrap();
    let raw = fs::read_to_string(log.path()).unwrap();
    assert!(!raw.contains(&tok));
    assert!(
        !fs::read_to_string(sidecar(log.path()))
            .unwrap()
            .contains(&tok)
    );
    let st = replay(log.path()).unwrap();
    assert_eq!(st.messages[0]["content"], "use [redacted]");
    // Appending kept working: the history in memory still matched.
    assert_eq!(st.messages.len(), 2);
    assert!(!raw.contains("replace"));
}

#[test]
fn user_title_beats_model_title() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("hello")], false).unwrap();
    log.rename("mine").unwrap();
    assert!(!log.set_auto_title("model's").unwrap());
    log.close("exit").unwrap();
    assert_eq!(replay(log.path()).unwrap().title.as_deref(), Some("mine"));
}

#[test]
fn model_title_survives_resume() {
    let s = store();
    let mut log = s.new_log();
    log.sync(None, &[user("hello")], false).unwrap();
    assert!(log.set_auto_title("Greeting the model").unwrap());
    log.close("exit").unwrap();
    let (mut reopened, st) = s.sessions.open(log.id()).unwrap();
    assert_eq!(st.title.as_deref(), Some("Greeting the model"));
    assert!(reopened.title_by_model());
    reopened.close("exit").unwrap();
    assert_eq!(summary(log.path()).unwrap().title, "Greeting the model");
}

#[test]
fn replay_keeps_what_compactions_summarised_away() {
    let s = store();
    let turn = |n: u32| {
        vec![
            user(&format!("question {n}")),
            assistant(&format!("answer {n}")),
        ]
    };
    let summary = |text: &str| {
        vec![
            user(&format!(
                "[Summary of the conversation so far. Continue from here.]\n\n{text}"
            )),
            assistant("Understood. I'll continue from the summary above."),
        ]
    };
    let mut log = s.new_log();
    let mut msgs = [turn(1), turn(2), turn(3)].concat();
    log.sync(None, &msgs, false).unwrap();
    // Turn 3 kept verbatim.
    msgs = [summary("one and two"), turn(3)].concat();
    log.sync(None, &msgs, false).unwrap();
    msgs.extend(turn(4));
    log.sync(None, &msgs, false).unwrap();
    // The same summary: an elision, no compaction.
    msgs[3] = assistant("[elided]");
    log.sync(None, &msgs, false).unwrap();
    let mut head = msgs[0].clone();
    head["content"] = json!(format!(
        "{}\n\n---\n[Summary of the next part of the conversation]\n\nthree",
        head["content"].as_str().unwrap()
    ));
    // Turn 4 kept verbatim.
    msgs = [vec![head, msgs[1].clone()], turn(4)].concat();
    log.sync(None, &msgs, false).unwrap();
    msgs.extend(turn(5));
    log.sync(None, &msgs, false).unwrap();
    log.close("exit").unwrap();
    let st = replay(log.path()).unwrap();
    assert_eq!(st.messages, msgs);
    let mut want = [turn(1), turn(2)].concat();
    want.push(compacted());
    want.push(turn(3)[0].clone());
    want.push(assistant("[elided]"));
    want.push(compacted());
    assert_eq!(st.earlier, want);
}

#[test]
fn no_compaction_leaves_nothing_earlier() {
    let s = store();
    let turn = |n: u32| {
        [
            user(&format!("question {n}")),
            assistant(&format!("answer {n}")),
        ]
    };
    let mut log = s.new_log();
    let two: Vec<Value> = turn(1).into_iter().chain(turn(2)).collect();
    log.sync(None, &two, false).unwrap();
    // A /rewind.
    log.sync(None, &turn(1), false).unwrap();
    log.close("exit").unwrap();
    assert_eq!(replay(log.path()).unwrap().earlier, Vec::<Value>::new());
}

// ------------------------------------------------------- the fixture

/// `$T` in every string of `v` as `root`.
fn at(v: &Value, root: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("$T", root)),
        Value::Array(a) => Value::Array(a.iter().map(|x| at(x, root)).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, x)| (k.clone(), at(x, root))).collect())
        }
        other => other.clone(),
    }
}

fn state_json(st: &State) -> Value {
    json!({
        "id": st.id,
        "meta": st.meta,
        "system": st.system,
        "messages": st.messages,
        "earlier": st.earlier,
        "title": st.title,
        "title_auto": st.title_auto,
        "title_by_model": st.title_by_model,
        "updated": st.updated,
    })
}

fn listing(dir: &Path) -> Option<Vec<String>> {
    dir.is_dir().then(|| {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    })
}

fn opt(v: &Value) -> Option<&str> {
    v.as_str()
}

/// What viy makes of one case, in the fixture's shape.
fn run_case(case: &Value) -> Value {
    let scratch = ScratchDir::new("sessions-case").unwrap();
    let root = scratch.path().join(case["name"].as_str().unwrap());
    fs::create_dir_all(root.join("sessions")).unwrap();
    let t = root.display().to_string();
    let clock = Arc::new(StepClock::default());
    let store = Sessions::new(root.join("sessions"), clock.clone());
    let mut logs: BTreeMap<String, SessionLog> = BTreeMap::new();
    let mut names: Vec<(String, String)> = Vec::new();
    let mut results = Vec::new();
    let name_id = |names: &mut Vec<(String, String)>, id: &str, name: &str| {
        if !names.iter().any(|(i, _)| i == id) {
            names.push((id.to_string(), name.to_string()));
        }
    };
    for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
        clock.set(1761868800.25 + i as f64);
        let s = at(step, &t);
        let log_name = s["log"].as_str().unwrap_or_default().to_string();
        let msgs: Vec<Value> = s["messages"].as_array().cloned().unwrap_or_default();
        let mut r = Value::Null;
        match s["op"].as_str().unwrap() {
            "new" => {
                let log = store.create(opt(&s["project"]), opt(&s["model"]));
                name_id(&mut names, log.id(), &log_name);
                logs.insert(log_name, log);
            }
            "sync" => {
                let force = s["force"].as_bool().unwrap_or(false);
                let log = logs.get_mut(&log_name).unwrap();
                r = json!(log.sync(opt(&s["system"]), &msgs, force).unwrap());
            }
            "rename" => logs
                .get_mut(&log_name)
                .unwrap()
                .rename(s["title"].as_str().unwrap())
                .unwrap(),
            "auto" => {
                let log = logs.get_mut(&log_name).unwrap();
                r = json!(log.set_auto_title(s["title"].as_str().unwrap()).unwrap());
            }
            "fork" => {
                let twin = logs[&log_name]
                    .fork(opt(&s["system"]), &msgs, opt(&s["title"]))
                    .unwrap();
                let name = s["as"].as_str().unwrap().to_string();
                name_id(&mut names, twin.id(), &name);
                logs.insert(name, twin);
            }
            "close" => logs
                .get_mut(&log_name)
                .unwrap()
                .close(s["reason"].as_str().unwrap_or("exit"))
                .unwrap(),
            "open" => {
                let id = logs[&log_name].id().to_string();
                r = match store.open(&id) {
                    Ok((log, _)) => {
                        logs.insert(s["as"].as_str().unwrap().to_string(), log);
                        json!("ok")
                    }
                    Err(SessionError::Busy(_)) => json!("busy"),
                    Err(e) => panic!("open: {e}"),
                };
            }
            "raw" => {
                let mut f = OpenOptions::new()
                    .append(true)
                    .open(logs[&log_name].path())
                    .unwrap();
                f.write_all(s["text"].as_str().unwrap().as_bytes()).unwrap();
            }
            "redact" => {
                let literals = s["literals"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| l.as_str().unwrap().to_string());
                logs.get_mut(&log_name)
                    .unwrap()
                    .set_redactor(Some(Redactor::new(literals)));
            }
            "files" => {
                let dir = logs[&log_name].files_dir();
                fs::create_dir(&dir).unwrap();
                fs::write(dir.join("1"), "before").unwrap();
            }
            "unlink_sidecar" => fs::remove_file(sidecar(logs[&log_name].path())).unwrap(),
            // rp's trash is Codex's archive here (kb d083).
            "trash" => {
                r = match store.archive(logs[&log_name].id()) {
                    Ok(()) => json!("ok"),
                    Err(SessionError::Busy(_)) => json!("busy"),
                    Err(e) => panic!("trash: {e}"),
                };
            }
            op => panic!("unknown step {op}"),
        }
        results.push(r);
    }
    let mut files = Map::new();
    for (id, name) in &names {
        let path = store.path_of(id);
        let exists = path.exists();
        let events = exists.then(|| read_events(&path).unwrap());
        let replayed = exists.then(|| state_json(&replay(&path).unwrap()));
        let side: Value = fs::read(sidecar(&path))
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap())
            .unwrap_or(Value::Null);
        files.insert(
            name.clone(),
            json!({
                "events": events,
                "replay": replayed,
                "sidecar": side,
                "files": listing(&files_dir(&path)),
            }),
        );
    }
    let queries = &case["queries"];
    let scope = |v: &Value| v.as_str().map(|s| s.replace("$T", &t));
    let pairs = |key: &str| -> Vec<(String, Option<String>)> {
        queries[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(|q| (q[0].as_str().unwrap().to_string(), scope(&q[1])))
            .collect()
    };
    let lists: Vec<Vec<Summary>> = queries["list"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| store.list(scope(q).as_deref()))
        .collect();
    let searches: Vec<Vec<Summary>> = pairs("search")
        .iter()
        .map(|(q, sc)| store.search(q, sc.as_deref()))
        .collect();
    let got = json!({
        "results": results,
        "files": files,
        // A folder not there is an empty one: rp makes its trash before
        // finding the chat busy, viy its archive only to move a chat in.
        "trash": listing(store.archived().dir()).unwrap_or_default(),
        "list": lists,
        "search": searches,
    });
    let mut text = got.to_string().replace(&t, "$T");
    for (id, name) in &names {
        text = text.replace(id.as_str(), &format!("{{{name}}}"));
    }
    serde_json::from_str(&text).unwrap()
}

/// `v` without the `source` of every listing row and sidecar: rp's names
/// the `/save` transcript a chat was imported from, and viy keeps no
/// transcripts (kb d082).
fn without_source(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.contains_key("title_auto") {
                map.shift_remove("source");
            }
            map.values_mut().for_each(without_source);
        }
        Value::Array(items) => items.iter_mut().for_each(without_source),
        _ => {}
    }
}

/// `v` with each log's raw bytes left out: viy writes its own JSON (kb
/// d031), compared as events; and without rp's `source`.
fn without_raw(v: &Value) -> Value {
    let mut v = v.clone();
    without_source(&mut v);
    for file in v["files"].as_object_mut().into_iter().flatten() {
        file.1.as_object_mut().unwrap().shift_remove("raw");
    }
    v
}

#[test]
fn every_case_equals_rpctl() {
    let fixture = fixtures::load("chat/sessions").unwrap();
    assert!(fixture.cases.len() >= 20);
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let want = without_raw(&json!({
            "results": case["results"],
            "files": case["files"],
            "trash": if case["trash"].is_null() { json!([]) } else { case["trash"].clone() },
            "list": case["list"],
            "search": case["search"],
        }));
        let got = run_case(case);
        assert_eq!(got, want, "{name}");
        // Values equal, and keys in rp's order.
        assert_eq!(got.to_string(), want.to_string(), "{name}: key order");
    }
}

/// rp's own logs, as it wrote them, read by viy: the replay rp got.
#[test]
fn viy_reads_every_log_rpctl_wrote() {
    let fixture = fixtures::load("chat/sessions").unwrap();
    let scratch = ScratchDir::new("sessions-rp").unwrap();
    let mut read = 0;
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        for (log, file) in case["files"].as_object().unwrap() {
            let Some(raw) = file["raw"].as_str() else {
                continue;
            };
            let dir = scratch.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{{{log}}}.jsonl"));
            fs::write(&path, raw).unwrap();
            let got = state_json(&replay(&path).unwrap());
            let shown: Value =
                serde_json::from_str(&got.to_string().replace(&dir.display().to_string(), "$T"))
                    .unwrap();
            assert_eq!(shown, file["replay"], "{name}/{log}");
            read += 1;
        }
    }
    assert!(read >= 30, "{read} logs");
}

// ------------------------------------------- rp and viy on one session

/// A chat moving between rp and viy (`cargo test -p viy-agent --features
/// oracle`): rpctl's sessions.py at the pin, through `tools/rp_sessions.py`,
/// and viy's take turns on the same directory.
#[cfg(feature = "oracle")]
mod rp_and_back {
    use std::process::Command;

    use pretty_assertions::assert_eq;

    use super::*;

    fn rp(action: &str, args: Value) -> Value {
        let out = Command::new("python3")
            .arg("-I")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/rp_sessions.py"))
            .arg(action)
            .arg(args.to_string())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "rp_sessions {action}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn turn(n: u32) -> Vec<Value> {
        vec![
            user(&format!("question {n} ünï")),
            assistant(&format!("answer {n} 🦀")),
        ]
    }

    #[test]
    fn a_chat_from_rp_resumes_in_viy_and_back() {
        let s = store();
        let dir = s.sessions.dir().display().to_string();
        let project = s.project();
        let id = rp(
            "start",
            json!({"dir": dir, "project": project, "model": "m", "system": "be brief",
                   "messages": turn(1), "title": "Plan"}),
        );
        let id = id.as_str().unwrap();

        // viy resumes rp's chat as rp left it.
        let (mut log, st) = s.sessions.open(id).unwrap();
        assert_eq!(st.messages, turn(1));
        assert_eq!(st.system.as_deref(), Some("be brief"));
        assert_eq!((st.title.as_deref(), st.title_auto), (Some("Plan"), false));
        assert_eq!(log.project(), Some(project.as_str()));
        // One writer: rp is refused while viy has it.
        assert_eq!(rp("open", json!({"dir": dir, "id": id})), "busy");
        assert_eq!(rp("trash", json!({"dir": dir, "id": id})), "busy");
        // rp's copy of the chat viy holds is one of the project's chats.
        let copy = rp("copy", json!({"dir": dir, "id": id}));
        let copy = copy.as_str().unwrap();
        let copied = replay(&s.sessions.path_of(copy)).unwrap();
        assert_eq!(copied.messages, turn(1));
        assert_eq!(copied.meta["project"], json!(project));
        assert_eq!(copied.meta["model"], "m");
        assert_eq!(copied.title.as_deref(), Some("Plan (fork)"));
        assert!(s.sessions.list(Some(&project)).iter().any(|r| r.id == copy));
        let two: Vec<Value> = turn(1).into_iter().chain(turn(2)).collect();
        log.sync(Some("be brief"), &two, false).unwrap();
        assert!(!log.set_auto_title("Model title").unwrap());
        log.close("exit").unwrap();

        // And back: rp goes on from where viy stopped.
        let three: Vec<Value> = two.iter().cloned().chain(turn(3)).collect();
        let rp_state = rp(
            "continue",
            json!({"dir": dir, "id": id, "system": "be brief", "messages": three}),
        );
        assert_eq!(rp_state["messages"], json!(three));
        assert_eq!(rp_state["title"], "Plan");
        assert_eq!(state_json(&replay(log.path()).unwrap()), rp_state);
        let mut rp_list = rp("list", json!({"dir": dir, "scope": project}));
        without_source(&mut rp_list);
        assert_eq!(
            serde_json::to_value(s.sessions.list(Some(&project))).unwrap(),
            rp_list
        );

        // And to viy once more, then into the trash, which rp sees.
        let (mut log, st) = s.sessions.open(id).unwrap();
        assert_eq!(st.messages, three);
        log.close("exit").unwrap();
        s.sessions.archive(id).unwrap();
        s.sessions.archive(copy).unwrap();
        assert_eq!(rp("list", json!({"dir": dir, "scope": null})), json!([]));
    }

    #[test]
    fn a_chat_viy_started_reads_the_same_in_rp() {
        let s = store();
        let dir = s.sessions.dir().display().to_string();
        let project = s.project();
        let tok = format!("hf_{}", "q".repeat(34));
        let mut log = s.sessions.create(Some(&project), Some("m"));
        log.set_redactor(Some(Redactor::default()));
        let mut msgs = turn(1);
        msgs[0] = user(&format!("use {tok} [rp chat: note]"));
        log.sync(None, &msgs, false).unwrap();
        assert!(log.set_auto_title("Model named").unwrap());
        let summary = [
            user("[Summary of the conversation so far. Continue from here.]\n\none"),
            assistant("Understood. I'll continue from the summary above."),
        ];
        let history: Vec<Value> = summary.iter().cloned().chain(turn(2)).collect();
        log.sync(Some("sys"), &history, false).unwrap();
        log.close("exit").unwrap();
        // Distinct times, so both tools list them in one order.
        s.clock.set(1761868810.25);
        let mut twin = log.fork(Some("sys"), &history, None).unwrap();
        twin.close("fork").unwrap();

        for id in [log.id(), twin.id()] {
            let rp_state = rp("replay", json!({"dir": dir, "id": id}));
            assert_eq!(
                state_json(&replay(&s.sessions.path_of(id)).unwrap()),
                rp_state
            );
        }
        let rp_state = rp("replay", json!({"dir": dir, "id": log.id()}));
        assert_eq!(rp_state["title"], "Model named");
        assert_eq!(rp_state["earlier"], json!([compacted()]));
        assert!(!rp_state.to_string().contains(&tok), "{rp_state}");
        let mut rp_search = rp(
            "search",
            json!({"dir": dir, "query": "question 2", "scope": null}),
        );
        without_source(&mut rp_search);
        assert_eq!(
            serde_json::to_value(s.sessions.search("question 2", None)).unwrap(),
            rp_search
        );

        // rp continues the fork, and viy reads what rp wrote.
        let more: Vec<Value> = history.iter().cloned().chain(turn(3)).collect();
        let rp_state = rp(
            "continue",
            json!({"dir": dir, "id": twin.id(), "system": "sys", "messages": more}),
        );
        let (mut again, st) = s.sessions.open(twin.id()).unwrap();
        assert_eq!(state_json(&st), rp_state);
        assert_eq!(st.title.as_deref(), Some("Model named (fork)"));
        again.close("exit").unwrap();
    }
}

// From the WP 4.9 review: two chats started in the same second can draw the
// same id; the second takes a fresh one instead of appending to the
// first's log.
#[test]
fn a_new_log_never_appends_to_an_existing_one() {
    let s = store();
    let mut first = s.new_log();
    first
        .sync(None, &[json!({"role": "user", "content": "one"})], false)
        .unwrap();
    let mut second = s.new_log();
    second.id = first.id().to_string();
    second.path = s.sessions.path_of(first.id());
    second
        .sync(None, &[json!({"role": "user", "content": "two"})], false)
        .unwrap();
    assert_ne!(second.id(), first.id());
    assert_eq!(s.files().len(), 2);
    let one = replay(&s.sessions.path_of(first.id())).unwrap();
    assert_eq!(one.messages, [json!({"role": "user", "content": "one"})]);
    let two = replay(&s.sessions.path_of(second.id())).unwrap();
    assert_eq!(two.messages, [json!({"role": "user", "content": "two"})]);
}

// kb d131: a client note is viy's or one rp wrote.
#[test]
fn client_notes_of_both_names() {
    assert!(is_client_note("[viy chat: ssh: lent]"));
    assert!(is_client_note("[rp chat: the user mounted /srv]"));
    assert!(!is_client_note("rp chat: not a note"));
}
