//! Which chat `viy resume` and `viy fork` open (kb d083), as Codex's
//! session selection: an id or exact name, `--last`, or the picker.

use chrono::TimeZone;
use chrono::Utc;
use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::time::FakeTime;

use super::*;

struct Rig {
    store: Sessions,
    time: Arc<FakeTime>,
    _scratch: ScratchDir,
}

fn rig() -> Rig {
    let scratch = ScratchDir::new("chat-target").unwrap();
    let time = Arc::new(FakeTime::at(
        Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap(),
    ));
    let store = Sessions::new(scratch.path().join("sessions"), time.clone());
    Rig {
        store,
        time,
        _scratch: scratch,
    }
}

impl Rig {
    /// A saved chat named `title` in `project`, a minute after the last;
    /// one-shot with `exec`.
    fn chat(&self, title: &str, project: &str, exec: bool) -> String {
        self.time.advance(std::time::Duration::from_secs(60));
        let mut log = self.store.create(Some(project), Some("m"));
        if exec {
            log.mark_exec();
        }
        log.sync(None, &[json!({"role": "user", "content": "hi"})], false)
            .unwrap();
        log.rename(title).unwrap();
        log.close("exit").unwrap();
        log.id().to_string()
    }

    fn target(&self, wanted: Wanted) -> Result<Option<OpenSession>, String> {
        target(&self.store, &wanted, "/p").map_err(|f| f.message().unwrap_or_default().to_string())
    }
}

fn opens(id: &str, fork: bool) -> Option<OpenSession> {
    Some(OpenSession {
        id: Some(id.into()),
        fork,
        ..OpenSession::default()
    })
}

#[test]
fn a_chat_named_by_id_or_name_opens_and_one_not_there_ends_viy() {
    let r = rig();
    let id = r.chat("pod costs", "/p", false);
    let by_id = Wanted {
        id: Some(id.clone()),
        ..Wanted::default()
    };
    assert_eq!(r.target(by_id), Ok(opens(&id, false)));
    let by_name = Wanted {
        id: Some("pod costs".into()),
        fork: true,
        ..Wanted::default()
    };
    assert_eq!(r.target(by_name), Ok(opens(&id, true)));
    let missing = Wanted {
        id: Some("nothing".into()),
        fork: true,
        ..Wanted::default()
    };
    assert_eq!(
        r.target(missing),
        Err(
            "No saved session found with ID nothing. Run `viy fork` without an ID to choose \
             from existing sessions."
                .into()
        )
    );
}

#[test]
fn last_takes_this_projects_newest_interactive_chat_or_none() {
    let r = rig();
    assert_eq!(
        r.target(Wanted {
            last: true,
            ..Wanted::default()
        }),
        Ok(None)
    );
    let here = r.chat("here", "/p", false);
    let one_shot = r.chat("exec", "/p", true);
    let elsewhere = r.chat("elsewhere", "/q", false);
    let last = |include_exec, all, fork| Wanted {
        last: true,
        include_exec,
        all,
        fork,
        ..Wanted::default()
    };
    let id = |w| r.target(w).unwrap().and_then(|o| o.id);
    assert_eq!(id(last(false, false, false)), Some(here.clone()));
    assert_eq!(id(last(true, false, false)), Some(one_shot));
    assert_eq!(id(last(false, true, false)), Some(elsewhere));
    // Codex's fork --last never takes a one-shot chat.
    assert_eq!(id(last(true, false, true)), Some(here));
}

#[test]
fn nothing_named_is_a_pick() {
    let r = rig();
    let wanted = Wanted {
        all: true,
        include_exec: true,
        ..Wanted::default()
    };
    assert_eq!(
        r.target(wanted),
        Ok(Some(OpenSession {
            id: None,
            all: true,
            include_exec: true,
            fork: false,
        }))
    );
}

/// rpctl test_features.py BackendTests::test_require_key_per_backend.
#[test]
fn require_key_per_backend() {
    let paths = Paths::under("/home/u");
    let mut cfg = Config::defaults();
    cfg.set("BACKEND", "openai");
    let refused = require_key(Backend::Openai, &cfg, &paths).unwrap_err();
    assert!(refused.message().unwrap().contains("ENDPOINT_API_KEY"));
    let mut cfg = Config::defaults();
    cfg.set("VLLM_API_KEY", "k");
    assert!(require_key(Backend::Runpod, &cfg, &paths).is_ok());
}
