//! The command line: rpctl's `build_parser`, as Codex's clap derive
//! (codex-rs/cli/src/main.rs). Subcommands, flags, defaults, choices, exit
//! statuses and rpctl's help strings are kept; argparse's help layout is
//! not (flags.md F31). Long options may be abbreviated to any unique prefix,
//! as argparse allows. Checked against `tests/fixtures/cli/flags.json`.

use std::ffi::OsString;

use clap::Args;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

use crate::model::ModelArgs;

#[derive(Debug, Parser)]
#[command(
    name = "viy",
    version,
    disable_help_subcommand = true,
    about = "viy - run Qwen3.8-27B-abliterated on a RunPod GPU pod and chat with it."
)]
struct Parsed {
    #[command(subcommand)]
    command: Option<Command>,
}

/// What `viy` was asked to do. `viy` alone opens the chat, as `codex`
/// does; rp asks for `rp chat` (kb d067).
#[derive(Debug)]
pub struct Cli {
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "mark the current folder as a project (.viy/)")]
    Init,
    #[command(about = "copy rp's config and model profiles to viy's (rp's stay in place)")]
    Migrate,
    #[command(about = "create/resume the pod and wait for vLLM")]
    Up {
        #[arg(long, default_value_t = 2700, help = "seconds to wait for readiness")]
        timeout: u64,
    },
    #[command(about = "pod state, GPU, hourly cost, vLLM readiness")]
    Status,
    #[command(about = "stage timings of recent boots against the budget")]
    Boots(BootsArgs),
    #[command(about = "read platform and container logs (no SSH required)")]
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    #[command(about = "stop the pod (resumable; ends GPU billing)")]
    Stop,
    #[command(about = "terminate the pod (weights stay on the volume)")]
    Down,
    #[command(
        about = "viy down if nothing streamed for IDLE_TTL_MIN + REAP_GRACE_MIN (backup for the pod's own idle watchdog)"
    )]
    Reap {
        #[arg(long, help = "run viy reap every 5 minutes with a systemd user timer")]
        install: bool,
        #[arg(long, help = "print nothing (for the timer)")]
        quiet: bool,
    },
    #[command(
        about = "after a timeout: close open chats, viy down, power off this PC",
        after_help = "viy shutdown 90m          in 90 minutes (also 1h30m, 45s, 2h; a bare number is minutes)\n\
                      viy shutdown 23:30        at the next 23:30\n\
                      viy shutdown --status     what is scheduled\n\
                      viy shutdown --cancel     cancel it\n\
                      Open chats are saved and closed, then viy down runs; the PC powers off\n\
                      only once viy down has confirmed no pod is running. Anything that can't\n\
                      happen is flagged now and, with a desktop notification, when it fires."
    )]
    Shutdown(ShutdownArgs),
    #[command(about = "SSH forward to vLLM (the encrypted path; keeps RunPod's proxy out)")]
    Tunnel(TunnelArgs),
    #[command(about = "interactive streaming chat")]
    Chat(ChatArgs),
    #[command(
        about = "list, search, show, rename or remove saved chats",
        after_help = "viy chats [QUERY]          list this project's chats, or search them\n\
                      viy chats show REF         print a chat as Markdown\n\
                      viy chats rename REF TITLE\n\
                      viy chats rm REF           move a chat to the trash\n\
                      viy chats import [DIR]     absorb old /save transcripts\n\
                      REF is a chat id, an id prefix, or a search query."
    )]
    Chats(ChatsArgs),
    #[command(about = "one-shot prompt (reads stdin if omitted)")]
    Ask(AskArgs),
    #[command(
        about = "per-turn metrics, alarms and traces of viy chat",
        after_help = "viy trace                 recent turns: rounds, tool calls, time, alarms\n\
                      viy trace alarms          recent alarms (loops, errors, no answer, ...)\n\
                      viy trace stats           totals over recent turns\n\
                      viy trace SESSION_ID      one chat's timeline: rounds, tool calls, helpers"
    )]
    Trace(TraceArgs),
    #[command(about = "command-line tools the model keeps between chats")]
    Toolbox(ToolboxArgs),
    #[command(about = "manage model profiles (for a model swap)")]
    Model(ModelArgs),
    #[command(
        about = "primary and backup region, the backup's idle countdown, and RunPod's A100 stock per region"
    )]
    Backup {
        #[arg(
            long,
            default_value_t = 1,
            help = "stock queries 5s apart (stock flickers; default 1)"
        )]
        samples: u32,
    },
    #[command(about = "manage the weights network volume")]
    Volume(VolumeArgs),
    /// Internal: collect a boot's logs next to its record; `viy up` starts
    /// it in the background (`viy_pod::wait::spawn_capture`).
    #[command(hide = true, name = "capture-boot")]
    CaptureBoot { boot_id: String, pod_id: String },
    /// Internal: developer tools, such as `viy dev drive`.
    #[command(hide = true)]
    Dev(DevArgs),
}

impl Command {
    /// The subcommand's name, as typed.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Migrate => "migrate",
            Self::Up { .. } => "up",
            Self::Status => "status",
            Self::Boots(_) => "boots",
            Self::Logs { .. } => "logs",
            Self::Stop => "stop",
            Self::Down => "down",
            Self::Reap { .. } => "reap",
            Self::Shutdown(_) => "shutdown",
            Self::Tunnel(_) => "tunnel",
            Self::Chat(_) => "chat",
            Self::Chats(_) => "chats",
            Self::Ask(_) => "ask",
            Self::Trace(_) => "trace",
            Self::Toolbox(_) => "toolbox",
            Self::Model(_) => "model",
            Self::Backup { .. } => "backup",
            Self::Volume(_) => "volume",
            Self::CaptureBoot { .. } => "capture-boot",
            Self::Dev(_) => "dev",
        }
    }
}

#[derive(Debug, Args)]
pub struct DevArgs {
    #[command(subcommand)]
    pub command: DevCommand,
}

#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// Runs a scenario's steps through the engine and prints its events as
    /// JSON lines (implementation plan 3.4).
    Drive { scenario: std::path::PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BootsAction {
    List,
    Show,
}

#[derive(Debug, Args)]
pub struct BootsArgs {
    #[arg(value_enum, default_value_t = BootsAction::List)]
    pub action: BootsAction,
    #[arg(help = "boot id (or part of it) for show; default latest")]
    pub boot: Option<String>,
    #[arg(short = 'n', default_value_t = 10, help = "how many to list")]
    pub n: usize,
}

#[derive(Debug, Args)]
pub struct ShutdownArgs {
    #[arg(help = "duration (90m, 1h30m) or clock time (23:30)")]
    pub when: Option<String>,
    #[arg(long, help = "close chats and viy down, but leave this PC on")]
    pub no_poweroff: bool,
    #[arg(
        short,
        long,
        help = "count down in this terminal instead of the background"
    )]
    pub foreground: bool,
    #[arg(long, help = "show the scheduled shutdown")]
    pub status: bool,
    #[arg(long, help = "cancel the scheduled shutdown")]
    pub cancel: bool,
    // Set by the scheduled run itself.
    #[arg(long, hide = true)]
    pub run_at: Option<String>,
}

#[derive(Debug, Args)]
pub struct TunnelArgs {
    #[arg(short, long, help = "detach instead of holding the terminal")]
    pub background: bool,
    #[arg(long, help = "stop a backgrounded tunnel")]
    pub stop: bool,
    #[arg(long, help = "show tunnel state and the pinned host key")]
    pub status: bool,
    #[arg(
        long,
        help = "drop the pinned SSH host key (only for a known-good change)"
    )]
    pub forget_key: bool,
}

#[derive(Debug, Default, Args)]
pub struct ChatArgs {
    #[arg(long, help = "override the endpoint base URL")]
    pub url: Option<String>,
    #[arg(long, help = "initial system prompt")]
    pub system: Option<String>,
    #[arg(
        short = 'c',
        long = "continue",
        help = "continue the most recent chat in this project"
    )]
    pub cont: bool,
    // `-r` alone is an empty query: pick from every chat.
    #[arg(
        short,
        long,
        value_name = "QUERY",
        num_args = 0..=1,
        default_missing_value = "",
        help = "pick a saved chat to continue (optionally searching)"
    )]
    pub resume: Option<String>,
}

#[derive(Debug, Args)]
pub struct ChatsArgs {
    pub words: Vec<String>,
    #[arg(short, long, help = "every project, not just this one")]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct AskArgs {
    pub prompt: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub system: Option<String>,
    #[arg(long, help = "also print the reasoning trace")]
    pub think: bool,
    #[arg(
        long,
        help = "keep this exchange as a saved chat (viy chats, viy chat -c)"
    )]
    pub save: bool,
}

#[derive(Debug, Args)]
pub struct TraceArgs {
    #[arg(default_value = "")]
    pub what: String,
    #[arg(
        short = 'n',
        help = "how many rows (default 30; a session's whole timeline)"
    )]
    pub n: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ToolboxAction {
    List,
    Show,
    Find,
    Check,
    Path,
    Add,
    Rm,
}

#[derive(Debug, Args)]
pub struct ToolboxArgs {
    #[arg(value_enum, default_value_t = ToolboxAction::List)]
    pub action: ToolboxAction,
    #[arg(help = "show/rm: a tool name; find: words; add: a directory")]
    pub arg: Vec<String>,
    #[arg(long, help = "add/rm: the global toolbox instead of this project's")]
    pub global: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VolumeAction {
    List,
    Create,
    Delete,
}

#[derive(Debug, Args)]
pub struct VolumeArgs {
    #[arg(value_enum)]
    pub action: VolumeAction,
    #[arg(long)]
    pub volume_id: Option<String>,
}

/// The parser, with argparse's abbreviated long options in every
/// subcommand (clap's setting does not propagate).
pub fn command() -> clap::Command {
    Parsed::command().mut_subcommands(|sub| sub.infer_long_args(true))
}

/// Parses `args` (without the program name). An `Err` is a usage error,
/// `--help` or `--version`, to be printed and exited with.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Cli, clap::Error> {
    let argv = std::iter::once(OsString::from("viy")).chain(args);
    let matches = command().try_get_matches_from(argv)?;
    let parsed = Parsed::from_arg_matches(&matches)?;
    Ok(Cli {
        command: parsed
            .command
            .unwrap_or_else(|| Command::Chat(ChatArgs::default())),
    })
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
