//! `viy chats` (rpctl `cmd_chats`): list, show, rename, rm, import.

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;

use super::*;

struct Rig {
    paths: Paths,
    project: PathBuf,
    store: Sessions,
    _scratch: ScratchDir,
}

fn rig() -> Rig {
    let scratch = ScratchDir::new("chats").unwrap();
    let paths = Paths::under(scratch.path().join("home"));
    let project = std::fs::canonicalize(scratch.path())
        .unwrap()
        .join("project");
    std::fs::create_dir_all(project.join(".viy")).unwrap();
    let store = Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider));
    Rig {
        paths,
        project,
        store,
        _scratch: scratch,
    }
}

impl Rig {
    /// A saved chat of this project: `text` answered `ok`.
    fn chat(&self, text: &str) -> String {
        let mut log = self
            .store
            .create(Some(&self.project.display().to_string()), Some("m"));
        log.sync(
            Some("be brief"),
            &[
                json!({"role": "user", "content": text}),
                json!({"role": "assistant", "content": "ok"}),
            ],
            false,
        )
        .unwrap();
        log.close("exit").unwrap();
        log.id().to_string()
    }

    fn run(&self, words: &[&str], all: bool, input: &str) -> Result<String, Failure> {
        let args = ChatsArgs {
            words: words.iter().map(ToString::to_string).collect(),
            all,
        };
        let mut out = Vec::new();
        let cfg = Config::defaults();
        cmd_chats(
            &args,
            &self.paths,
            &cfg,
            &self.project,
            &mut input.as_bytes(),
            &mut out,
        )
        .map(|()| String::from_utf8(out).unwrap())
    }
}

#[test]
fn list_and_search() {
    let r = rig();
    assert_eq!(
        r.run(&[], false, "").unwrap(),
        "no saved chats in this project (--all for every project)\n"
    );
    let id = r.chat("apples");
    let out = r.run(&["list"], false, "").unwrap();
    assert_eq!(out, format!("   1. apples  just now · 2 msgs · {id}\n"));
    assert_eq!(
        r.run(&["pears"], true, "").unwrap(),
        "no saved chats matching 'pears' anywhere\n"
    );
    let out = r.run(&["apples"], true, "").unwrap();
    assert!(out.ends_with(&format!("{id}  this project\n")), "{out}");
}

#[test]
fn show_prints_the_chat_as_markdown() {
    let r = rig();
    r.chat("apples");
    assert_eq!(
        r.run(&["show", "apples"], false, "").unwrap(),
        "# apples\n\n## system\n\nbe brief\n\n## user\n\napples\n\n## assistant\n\nok\n\n"
    );
    assert_eq!(
        r.run(&["show"], false, "").err().map(|f| f.message),
        Some(Some("usage: viy chats show REF".into()))
    );
}

#[test]
fn a_ref_that_matches_several_lists_them() {
    let r = rig();
    r.chat("apples one");
    r.chat("apples two");
    let failed = r.run(&["show", "apples"], false, "");
    assert!(matches!(&failed, Err(f) if f.code == 1 && f.message.is_none()));
    let r2 = rig();
    assert_eq!(
        r2.run(&["show", "nothing"], false, "")
            .err()
            .map(|f| f.message),
        Some(Some("no saved chat matches 'nothing'".into()))
    );
}

#[test]
fn rename_and_rm() {
    let r = rig();
    let id = r.chat("apples");
    assert_eq!(
        r.run(&["rename", "apples", "Fruit", "plan"], false, "")
            .unwrap(),
        format!("renamed {id} to 'Fruit plan'\n")
    );
    assert_eq!(r.store.list(None)[0].title, "Fruit plan");
    let out = r.run(&["rm", "Fruit"], false, "n\n").unwrap();
    assert_eq!(out, "move 'Fruit plan' (2 msgs) to the trash? [y/N] ");
    assert_eq!(r.store.list(None).len(), 1);
    let out = r.run(&["rm", "Fruit"], false, "y\n").unwrap();
    assert!(out.ends_with("/trash/\n"), "{out}");
    assert_eq!(r.store.list(None), vec![]);
}

#[test]
fn a_chat_open_elsewhere_is_not_renamed_or_removed() {
    let r = rig();
    let id = r.chat("apples");
    let (held, _) = r.store.open(&id).unwrap();
    assert_eq!(
        r.run(&["rename", "apples", "x"], false, "")
            .err()
            .map(|f| f.message),
        Some(Some(
            "that chat is open in a viy chat; use /rename there".into()
        ))
    );
    assert_eq!(
        r.run(&["rm", "apples"], false, "y\n")
            .err()
            .map(|f| f.message),
        Some(Some(
            "that chat is open in a viy chat; quit it first".into()
        ))
    );
    drop(held);
}

#[test]
fn import_turns_saved_transcripts_into_chats_once() {
    let r = rig();
    let dir = r.project.join(".viy").join("transcripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("notes.md"),
        "## user\n\nhi\n\n<!-- rpctl-state\n{\"system\": null, \"messages\": \
         [{\"role\": \"user\", \"content\": \"hi\"}]}\n-->\n",
    )
    .unwrap();
    std::fs::write(dir.join("plain.md"), "## user\n\nhi\n").unwrap();
    let out = r.run(&["import"], false, "").unwrap();
    let id = &r.store.list(None)[0].id;
    assert_eq!(
        out,
        format!(
            "imported notes.md -> {id}\nskipped plain.md: no rpctl state block\n1 imported; the \
             .md files are untouched.\n"
        )
    );
    assert_eq!(
        r.store.list(None)[0].project.as_deref(),
        Some(r.project.display().to_string().as_str())
    );
    let again = r.run(&["import"], false, "").unwrap();
    assert!(
        again.ends_with("0 imported; the .md files are untouched.\n"),
        "{again}"
    );
    assert_eq!(
        r.run(&["import", "/nonexistent/x"], false, "").unwrap(),
        "no transcripts in /nonexistent/x\n"
    );
}
