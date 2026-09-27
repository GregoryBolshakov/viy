//! Slash-command completion for the input box (ui-plan 5): the names, and
//! the values of the commands that take a fixed set, offered in a popup
//! above the input as Codex's `bottom_pane/command_popup.rs` does.
//!
//! The commands and their values are rp chat's `HELP` (rpctl b6de7f1
//! chat.py 534-595), aliases left out. Running them is the engine's (WP
//! 3.12); the pod commands join with WP 3.13 (kb d068).

/// A slash command as `/help` lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// Without the `/`.
    pub name: &'static str,
    /// The argument as `/help` shows it, `""` for none.
    pub args: &'static str,
    /// One line of `/help`'s text.
    pub about: &'static str,
    /// The values its argument takes, when they are a fixed set.
    pub values: &'static [&'static str],
}

const ON_OFF: &[&str] = &["on", "off"];

pub const COMMANDS: &[Command] = &[
    cmd("help", "", "show this", &[]),
    cmd("new", "", "start a fresh chat (this one stays saved)", &[]),
    cmd("system", "<text>", "set the system prompt", &[]),
    cmd("rename", "[title]", "name this chat", &[]),
    cmd(
        "rewind",
        "[n]",
        "go back to before one of your messages",
        &[],
    ),
    cmd("fork", "[title]", "go on in a copy of this chat", &[]),
    cmd("chats", "[query]", "list / search saved chats", &[]),
    cmd("resume", "[query]", "switch to a saved chat", &[]),
    cmd(
        "image",
        "<path|url>",
        "attach an image to the next message",
        &[],
    ),
    cmd("images", "", "list pending attachments", &[]),
    cmd("temp", "<float>", "sampling temperature", &[]),
    cmd(
        "effort",
        "[xhigh|medium|low]",
        "how hard the model thinks",
        &["xhigh", "medium", "low"],
    ),
    cmd("maxtok", "<int>", "max tokens per reply", &[]),
    cmd("think", "on|off", "stream the reasoning trace", ON_OFF),
    cmd("md", "on|off", "render Markdown in replies", ON_OFF),
    cmd(
        "search",
        "[on|off]",
        "let the model search and read the web",
        ON_OFF,
    ),
    cmd("tools", "on|off", "let the model build", ON_OFF),
    cmd(
        "instructions",
        "[on|off]",
        "show or load AGENTS.md / QWEN.md / CLAUDE.md",
        ON_OFF,
    ),
    cmd(
        "mode",
        "[normal|auto-yes]",
        "who answers the approver's questions",
        &["normal", "auto-yes"],
    ),
    cmd(
        "sources",
        "",
        "list the URLs from this turn's searches",
        &[],
    ),
    cmd("compact", "[full] [focus]", "shrink the context", &[]),
    cmd(
        "autocompact",
        "on|off|<pct>",
        "compact before the context spills",
        ON_OFF,
    ),
    cmd(
        "consult",
        "[on|off]",
        "ask_claude: hand hard tasks to Claude Code",
        ON_OFF,
    ),
    cmd(
        "toolbox",
        "[list|show|find|check|on|off]",
        "command-line tools kept between chats",
        &["list", "show", "find", "check", "on", "off"],
    ),
    cmd(
        "mount",
        "[<drive|path> [rw]]",
        "lend the model a drive or folder",
        &[],
    ),
    cmd(
        "unmount",
        "[<drive|path>|all]",
        "take a mounted drive back",
        &[],
    ),
    cmd(
        "ssh",
        "[<host> [key file] [full]]",
        "lend the model a server",
        &[],
    ),
    cmd("unssh", "[<host>|all]", "take a server back", &[]),
    cmd(
        "git",
        "[init|import|reset]",
        "the model's own git repository",
        &["init", "import", "reset"],
    ),
    cmd("stats", "", "last response timing", &[]),
    cmd("exit", "", "quit", &[]),
];

const fn cmd(
    name: &'static str,
    args: &'static str,
    about: &'static str,
    values: &'static [&'static str],
) -> Command {
    Command {
        name,
        args,
        about,
        values,
    }
}

/// One offer in the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The input once it is taken.
    pub text: String,
    /// What the popup shows on the left.
    pub label: String,
    /// What it shows on the right.
    pub about: &'static str,
}

/// What the popup offers for the input `text`, with the cursor at its end:
/// the commands whose name starts with what follows `/`, or, after a
/// command and a space, its values that start with what follows. Nothing
/// once a name or value is typed out in full, on a second word, or on a
/// second line.
pub fn completions(text: &str) -> Vec<Completion> {
    let Some(rest) = text.strip_prefix('/') else {
        return Vec::new();
    };
    if rest.contains('\n') {
        return Vec::new();
    }
    match rest.split_once(' ') {
        None => COMMANDS
            .iter()
            .filter(|c| c.name.starts_with(rest) && c.name != rest)
            .map(|c| Completion {
                text: if c.args.is_empty() {
                    format!("/{}", c.name)
                } else {
                    format!("/{} ", c.name)
                },
                label: if c.args.is_empty() {
                    format!("/{}", c.name)
                } else {
                    format!("/{} {}", c.name, c.args)
                },
                about: c.about,
            })
            .collect(),
        Some((name, typed)) if !typed.contains(' ') => {
            let Some(command) = COMMANDS.iter().find(|c| c.name == name) else {
                return Vec::new();
            };
            command
                .values
                .iter()
                .filter(|v| v.starts_with(typed) && **v != typed)
                .map(|v| Completion {
                    text: format!("/{name} {v}"),
                    label: (*v).to_string(),
                    about: command.about,
                })
                .collect()
        }
        Some(_) => Vec::new(),
    }
}

#[cfg(test)]
#[path = "slash_tests.rs"]
mod tests;
