//! `viy chats` (rpctl `cmd_chats`): list and search the saved chats, and
//! `show`, `rename` and `rm` one, with rp's words. rp's `import` of old
//! `/save` transcripts is not kept: viy keeps no transcripts (kb d082).

use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use viy_agent::chat::render_content;
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
        .filter(|w| matches!(*w, "show" | "rename" | "rm" | "list"));
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
        // `rm`.
        Some(_) => {
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
    }
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
