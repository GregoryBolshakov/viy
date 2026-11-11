//! The chat's side of the model's own git repository (rp's `_private_git`,
//! `_sandbox_git`, `_sbgit_password`, `_cmd_git`, `_git_init`; rpctl kb
//! d026): whether the chat offers it, preparing it on the first sandboxed
//! command, and `/git`.
//!
//! The tok PIN, when the key is PIN-gated or `/git init` must add it, is
//! asked as a secret ([`TurnContext::ask_secret`]): the UI shows none of
//! it, and it goes only into tok's environment (kb d128).

use std::path::PathBuf;

use viy_protocol::Answer;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_tools::sandboxgit;
use viy_tools::sandboxgit::Runner;
use viy_tools::sandboxgit::TOKEN;
use viy_tools::sandboxgit::Unavailable;
use viy_tools::untrusted;

use super::Chat;
use super::notice;
use crate::engine::TurnContext;

/// The PIN question for opening the repository.
pub const PIN_TO_OPEN: &str = "  tok PIN to open the model's git repository (Enter skips):";

impl Chat {
    /// Decides once whether this chat offers the model its own .git (rp's
    /// `_private_git`): SANDBOX_GIT, a workspace with a real .git, the
    /// sandbox, and the repository set up.
    pub(crate) async fn decide_private_git(&mut self) -> bool {
        if let Some(offer) = self.sbgit_offer {
            return offer;
        }
        let ws = self.places.workspace.clone();
        let offer = self.sandbox_git_on
            && sandboxgit::applies(&ws)
            && self.auto_ready().await.is_ok()
            && self.git_problem().await.is_none();
        self.sbgit_offer = Some(offer);
        offer
    }

    /// [`sandboxgit::problem`] off the async runtime (it asks tok).
    async fn git_problem(&self) -> Option<String> {
        let (ws, places, tok) = (
            self.places.workspace.clone(),
            self.places.sandbox_git.clone(),
            self.tok.clone(),
        );
        tokio::task::spawn_blocking(move || sandboxgit::problem(&ws, &places, &tok))
            .await
            .unwrap_or_else(|e| Some(e.to_string()))
    }

    /// The model's repository for the sandbox's `git_dir`, prepared on the
    /// first command of the chat (rp's `_sandbox_git`); `None` when it is
    /// not offered or preparing it failed.
    pub(crate) async fn sandbox_git(&mut self, ctx: &TurnContext) -> Option<PathBuf> {
        if !self.decide_private_git().await || self.sbgit_failed.is_some() {
            return None;
        }
        if let Some(repo) = &self.sbgit {
            return Some(repo.clone());
        }
        let host = self.sandbox_host();
        let project = self.places.project_dir.clone();
        let places = self.places.sandbox_git.clone();
        let ws = self.places.workspace.clone();
        let tok = self.tok.clone();
        let model = self.settings.model.clone();
        let author = format!(
            "{} (viy chat)",
            std::path::Path::new(&model)
                .file_name()
                .map_or(model.as_str().into(), |n| n.to_string_lossy())
        );
        let runner = Runner {
            host: &host,
            launcher: self.sandbox_launcher,
            project_dir: &project,
        };
        let got = sandboxgit::prepare(&runner, &places, &ws, || password(ctx, &tok), &author).await;
        self.sbgit_used = self.sbgit_used || places.mounted();
        match got {
            Ok(repo) => {
                notice(
                    ctx,
                    NoticeLevel::Info,
                    "git",
                    "  [git] .git in the sandbox is the model's own repository; /git lists its \
                     branches, /git import brings them into yours",
                );
                self.sbgit = Some(repo.clone());
                Some(repo)
            }
            Err(e) => {
                notice(
                    ctx,
                    NoticeLevel::Error,
                    "git",
                    format!(
                        "  [git] the model's own repository is unavailable: {e}. Its .git is \
                         read-only for this chat (/git retries)"
                    ),
                );
                self.sbgit_failed = Some(e.0);
                None
            }
        }
    }

    /// Why preparing the repository failed, for run_shell's `git_note`.
    pub(crate) fn sandbox_git_failed(&self) -> Option<&str> {
        self.sbgit_failed.as_deref()
    }

    /// Whether the repository is offered, for the network reviewer.
    pub(crate) fn private_git(&self) -> bool {
        self.sbgit_offer == Some(true)
    }

    /// The repository once prepared, for explore's read-only helpers.
    pub(crate) fn prepared_git(&self) -> Option<PathBuf> {
        self.sbgit.clone()
    }

    /// `/git`: the model's repository (rp's `_cmd_git`).
    pub(crate) async fn git_command(&mut self, ctx: &TurnContext, arg: &str) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "git", text);
        let words: Vec<String> = arg.split_whitespace().map(str::to_string).collect();
        let sub = words.first().map_or("", String::as_str);
        if sub == "init" {
            self.git_init(ctx).await;
            return;
        }
        if !matches!(sub, "" | "import" | "reset") {
            say(
                NoticeLevel::Warn,
                "usage: /git [init | import [branch...] | reset]".into(),
            );
            return;
        }
        if !self.decide_private_git().await {
            let why = if !self.sandbox_git_on {
                "SANDBOX_GIT=0".to_string()
            } else if self.auto_ready().await.is_err() {
                "the sandbox is unavailable".to_string()
            } else {
                self.git_problem()
                    .await
                    .unwrap_or_else(|| "it is off for this chat".into())
            };
            say(
                NoticeLevel::Info,
                format!("the model's .git is your read-only one: {why}"),
            );
            return;
        }
        self.sbgit_failed = None; // /git is how you retry
        let Some(repo) = self.sandbox_git(ctx).await else {
            return;
        };
        let host = self.sandbox_host();
        let project = self.places.project_dir.clone();
        let ws = self.places.workspace.clone();
        let places = self.places.sandbox_git.clone();
        let runner = Runner {
            host: &host,
            launcher: self.sandbox_launcher,
            project_dir: &project,
        };
        let done: Result<(), Unavailable> = match sub {
            "reset" => {
                let q = Question::yes_no(
                    None,
                    "  delete the model's repository for this workspace, with every branch you \
                     have not imported?",
                );
                if ctx.ask(&q).await != Answer::Yes {
                    say(NoticeLevel::Info, "  [kept]".into());
                    return;
                }
                match sandboxgit::discard(&places, &ws) {
                    Ok(()) => {
                        self.sbgit = None;
                        if self.sandbox_git(ctx).await.is_some() {
                            say(
                                NoticeLevel::Info,
                                "  started over from your repository".into(),
                            );
                        }
                        Ok(())
                    }
                    Err(e) => Err(Unavailable(e.to_string())),
                }
            }
            "import" => match self.sandbox_tmp.path() {
                Err(e) => Err(Unavailable(format!("no /tmp for the sandbox: {e}"))),
                Ok(tmp) => {
                    let tmp = tmp.to_path_buf();
                    sandboxgit::import_branches(&runner, &ws, &repo, &tmp, &words[1..])
                        .await
                        .map(|report| {
                            // branch names are the model's: cleaned as rp's for_terminal
                            let report = untrusted::clean(report.trim_end());
                            if report.trim().is_empty() {
                                say(NoticeLevel::Info, "already up to date".into());
                            } else {
                                say(NoticeLevel::Info, report);
                            }
                            say(
                                NoticeLevel::Info,
                                "  review with git log -p HEAD..sandbox/<branch>, then merge or \
                                 cherry-pick"
                                    .into(),
                            );
                        })
                }
            },
            _ => sandboxgit::branches(&runner, &ws, &repo).await.map(|rows| {
                for (n, name) in &rows {
                    let ahead = if *n == 0 {
                        "nothing new".to_string()
                    } else {
                        format!("{n} commit{} yours lacks", if *n == 1 { "" } else { "s" })
                    };
                    say(
                        NoticeLevel::Info,
                        format!("  {}  {ahead}", untrusted::terminal_safe(name, Some(200))),
                    );
                }
                if rows.is_empty() {
                    say(NoticeLevel::Info, "  no branches yet".into());
                }
                say(
                    NoticeLevel::Info,
                    format!(
                        "  encrypted in {}; /git import brings branches into yours as \
                         sandbox/<branch>",
                        places.cipher.display()
                    ),
                );
            }),
        };
        if let Err(e) = done {
            say(NoticeLevel::Error, format!("  [git] {e}"));
        }
    }

    /// `/git init`: the encrypted directory and its tok key (rp's
    /// `_git_init`).
    async fn git_init(&mut self, ctx: &TurnContext) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "git", text);
        let places = self.places.sandbox_git.clone();
        if places.initialized() {
            say(
                NoticeLevel::Info,
                format!("already set up: {}", places.cipher.display()),
            );
            return;
        }
        let tok = self.tok.clone();
        let state = {
            let tok = tok.clone();
            tokio::task::spawn_blocking(move || sandboxgit::token_state(&tok))
                .await
                .unwrap_or(None)
        };
        let mut pin = None;
        if state != Some(true) {
            let what = if state.is_some() { "to read" } else { "to add" };
            match ctx
                .ask_secret(&format!("  tok PIN ({what} {TOKEN}):"))
                .await
            {
                Some(p) => pin = Some(p),
                None => {
                    say(NoticeLevel::Info, "  [cancelled]".into());
                    return;
                }
            }
        }
        let mut open = false;
        if state.is_none() {
            let q = Question::yes_no(
                None,
                format!(
                    "  let tok read {TOKEN} without the PIN (open tier), so chats open the \
                     repository without asking?"
                ),
            );
            open = ctx.ask(&q).await == Answer::Yes;
        }
        let made = sandboxgit::init_vault(
            &places,
            &tok,
            state.is_some(),
            pin.as_ref().map(viy_protocol::Secret::expose),
            open,
        )
        .await;
        drop(pin);
        if let Err(e) = made {
            say(NoticeLevel::Error, format!("  [git] {e}"));
            return;
        }
        let was = self.sbgit_offer;
        self.sbgit_offer = None;
        self.sbgit_failed = None;
        let tier = if open || state == Some(true) {
            "open"
        } else {
            "PIN-gated"
        };
        say(
            NoticeLevel::Info,
            format!(
                "  set up: {}, key {TOKEN} ({tier})",
                places.cipher.display()
            ),
        );
        if self.decide_private_git().await && was == Some(false) {
            say(
                NoticeLevel::Info,
                "  run_shell's description changed, so the next request re-reads the whole chat \
                 once"
                    .into(),
            );
        }
    }
}

/// The repository's key (rp's `_sbgit_password`): read without a PIN when
/// the token is open, else with the PIN the user gives; none given, none
/// read.
async fn password(ctx: &TurnContext, tok: &viy_config::vault::Tok) -> Result<String, Unavailable> {
    let state = {
        let tok = tok.clone();
        tokio::task::spawn_blocking(move || sandboxgit::token_state(&tok))
            .await
            .unwrap_or(None)
    };
    let pin = if state == Some(true) {
        None
    } else {
        match ctx.ask_secret(PIN_TO_OPEN).await {
            Some(pin) => Some(pin),
            None => return Err(Unavailable("no PIN given".into())),
        }
    };
    let tok = tok.clone();
    tokio::task::spawn_blocking(move || {
        sandboxgit::read_password(&tok, pin.as_ref().map(viy_protocol::Secret::expose))
    })
    .await
    .unwrap_or_else(|e| Err(Unavailable(e.to_string())))
}
