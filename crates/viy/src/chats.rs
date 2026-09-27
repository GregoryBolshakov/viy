//! `viy chats` (rpctl `cmd_chats`): list and search the saved chats, and
//! `show`, `rename`, `rm` and `import` one, with rp's words.

use std::collections::HashSet;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use viy_agent::chat::render_content;
use viy_agent::chat::state_block;
use viy_agent::sessions;
use viy_agent::sessions::RowView;
use viy_agent::sessions::SessionError;
use viy_agent::sessions::Sessions;
use viy_agent::sessions::Summary;
use viy_config::Config;
use viy_config::Paths;
use viy_config::Project;
use viy_config::redact::Redactor;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::cli::ChatsArgs;

/// Where `viy chats` looks.
struct Here<'a> {
    store: Sessions,
    project: Project,
    home: &'a Path,
}

impl Here<'_> {
    fn root(&self) -> String {
        self.project.root().display().to_string()
    }

    fn scope(&self, all: bool) -> Option<String> {
        (!all).then(|| self.root())
    }

    fn listing(&self, rows: &[Summary], all: bool) -> Vec<String> {
        let root = self.root();
        let view = RowView {
            all_projects: all,
            current: None,
            here: Some(&root),
            home: self.home,
            now: self.store.now(),
        };
        sessions::listing(rows, &view)
    }

    /// `reference` as exactly one chat (rp's `_one_chat`): this project's
    /// first, then every project's; several are listed.
    fn one_chat(
        &self,
        reference: &str,
        all: bool,
        out: &mut dyn Write,
    ) -> Result<Summary, Failure> {
        let mut rows = self.store.resolve(reference, self.scope(all).as_deref());
        if rows.is_empty() && !all {
            rows = self.store.resolve(reference, None);
        }
        match rows.len() {
            1 => Ok(rows.swap_remove(0)),
            0 => Err(Failure::from(format!(
                "no saved chat matches '{reference}'"
            ))),
            n => {
                let _ = writeln!(out, "'{reference}' matches {n} chats; use an id:");
                for line in self.listing(&rows, true) {
                    let _ = writeln!(out, "{line}");
                }
                Err(Failure::silent(1))
            }
        }
    }
}

/// Runs `viy chats`.
pub fn cmd_chats(
    args: &ChatsArgs,
    paths: &Paths,
    cfg: &Config,
    cwd: &Path,
    input: &mut dyn BufRead,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let here = Here {
        store: Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider)),
        project: Project::find(cwd).map_err(|e| Failure::from(e.to_string()))?,
        home: &paths.home,
    };
    let words = &args.words;
    let action = words
        .first()
        .map(String::as_str)
        .filter(|w| matches!(*w, "show" | "rename" | "rm" | "import" | "list"));
    let rest: &[String] = if action.is_some() { &words[1..] } else { words };
    let joined = rest.join(" ");
    let say = |out: &mut (dyn Write + Send), text: String| {
        writeln!(out, "{text}").map_err(|e| Failure::from(e.to_string()))
    };
    match action {
        None | Some("list") => {
            let rows = here.store.find(&joined, here.scope(args.all).as_deref());
            if rows.is_empty() {
                let place = if args.all {
                    "anywhere"
                } else {
                    "in this project (--all for every project)"
                };
                let what = if joined.is_empty() {
                    String::new()
                } else {
                    format!("matching '{joined}' ")
                };
                return say(out, format!("no saved chats {what}{place}"));
            }
            for line in here.listing(&rows, args.all) {
                say(out, line)?;
            }
            Ok(())
        }
        Some("show") => {
            if rest.is_empty() {
                return Err("usage: viy chats show REF".into());
            }
            let row = here.one_chat(&joined, args.all, out)?;
            let st = sessions::replay(&here.store.path_of(&row.id))
                .map_err(|e| Failure::from(e.to_string()))?;
            say(out, format!("# {}\n", row.title))?;
            if let Some(system) = st.system.as_deref().filter(|s| !s.is_empty()) {
                say(out, format!("## system\n\n{system}\n"))?;
            }
            for m in &st.messages {
                say(
                    out,
                    format!(
                        "## {}\n\n{}\n",
                        m["role"].as_str().unwrap_or_default(),
                        render_content(m)
                    ),
                )?;
            }
            Ok(())
        }
        Some("rename") => {
            if rest.len() < 2 {
                return Err("usage: viy chats rename REF NEW TITLE".into());
            }
            let row = here.one_chat(&rest[0], args.all, out)?;
            let (mut log, _) = here.store.open(&row.id).map_err(|e| match e {
                SessionError::Busy(_) => {
                    Failure::from("that chat is open in a viy chat; use /rename there")
                }
                SessionError::Io(e) => Failure::from(e.to_string()),
            })?;
            let title = rest[1..].join(" ");
            if cfg.get("CHAT_REDACT") == Some("1") {
                log.set_redactor(Some(Redactor::from_config(cfg)));
            }
            log.rename(&title)
                .and_then(|()| log.close("rename"))
                .map_err(|e| Failure::from(e.to_string()))?;
            say(out, format!("renamed {} to '{title}'", row.id))
        }
        Some("rm") => {
            if rest.is_empty() {
                return Err("usage: viy chats rm REF".into());
            }
            let row = here.one_chat(&joined, args.all, out)?;
            write!(
                out,
                "move '{}' ({} msgs) to the trash? [y/N] ",
                row.title, row.messages
            )
            .and_then(|()| out.flush())
            .map_err(|e| Failure::from(e.to_string()))?;
            let mut answer = String::new();
            let _ = input.read_line(&mut answer);
            if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
                return Ok(());
            }
            here.store.trash(&row.id).map_err(|e| match e {
                SessionError::Busy(_) => {
                    Failure::from("that chat is open in a viy chat; quit it first")
                }
                SessionError::Io(e) => Failure::from(e.to_string()),
            })?;
            say(
                out,
                format!("moved to {}/", here.store.dir().join("trash").display()),
            )
        }
        Some(_) => {
            let dir = match rest.first() {
                Some(dir) => expand_home(dir, here.home),
                None => here.project.transcripts_dir(),
            };
            import(&here, &dir, out)
        }
    }
}

/// `~` and `~/...` under `home`.
fn expand_home(text: &str, home: &Path) -> PathBuf {
    match text.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(text),
    }
}

/// `viy chats import [dir]`: old `/save` transcripts as saved chats; one
/// imported before is skipped, the `.md` files left in place (rp's
/// `cmd_chats_import`).
fn import(here: &Here<'_>, dir: &Path, out: &mut (dyn Write + Send)) -> Result<(), Failure> {
    let mut say = |text: String| writeln!(out, "{text}").map_err(|e| Failure::from(e.to_string()));
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    if files.is_empty() {
        return say(format!("no transcripts in {}", dir.display()));
    }
    let done: HashSet<String> = here
        .store
        .list(None)
        .into_iter()
        .filter_map(|r| r.source)
        .collect();
    // A project's own transcripts are filed under that project.
    let owner = dir
        .parent()
        .filter(|d| {
            d.file_name()
                .is_some_and(|n| n == viy_config::project::MARKER)
        })
        .and_then(Path::parent)
        .and_then(|root| Project::find(root).ok());
    let project = owner.map_or_else(|| here.root(), |p| p.root().display().to_string());
    let mut n = 0;
    for file in files {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let resolved = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        if done.contains(&resolved.display().to_string()) {
            continue;
        }
        let bytes = std::fs::read(&file).map_err(|e| Failure::from(e.to_string()))?;
        let text = String::from_utf8_lossy(&bytes);
        let Some(block) = state_block(&text) else {
            say(format!("skipped {name}: no rpctl state block"))?;
            continue;
        };
        let Ok(state) = serde_json::from_str::<serde_json::Value>(block) else {
            say(format!("skipped {name}: unreadable state block"))?;
            continue;
        };
        let messages = state["messages"].as_array().cloned().unwrap_or_default();
        let id = here
            .store
            .import_transcript(&file, state["system"].as_str(), &messages, Some(&project))
            .map_err(|e| Failure::from(e.to_string()))?;
        say(format!("imported {name} -> {id}"))?;
        n += 1;
    }
    say(format!("{n} imported; the .md files are untouched."))
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
