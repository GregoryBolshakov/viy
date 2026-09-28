//! Where the model is, as the chat sees it (kb d041): the chat opens
//! whether or not anything answers, asks its [`Server`] before each turn
//! where to send it, and hands it the pod commands, which run beside the
//! conversation. The model and its address come with each turn, so a pod
//! that came up after the chat opened, in this chat or in another terminal,
//! is the one the turn goes to, and a new address or model gets its
//! context length asked again (Codex's model per turn,
//! `core/src/session/turn_context.rs`).
//!
//! The pod's own implementation is the `viy` crate's, on `viy_pod`'s
//! library calls; a chat without a server (`viy dev`, `viy exec`) talks to
//! the address it was made with.

use std::sync::Arc;

use crate::engine::BoxFuture;
use crate::engine::TurnContext;

/// What a turn is sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The OpenAI-compatible base URL, without `/v1` or a trailing `/`.
    pub base: String,
    /// The model id requests name.
    pub model: String,
}

/// The model's server, watched for the chat.
pub trait Server: Send + Sync + 'static {
    /// Where a turn goes now, or why nothing would answer it (said to the
    /// user, whose line is given back).
    fn ready(self: Arc<Self>) -> BoxFuture<'static, Result<Ready, String>>;

    /// Starts `/name args` in the background if it is one of the server's
    /// commands (`/up`, `/status`, ...; kb d068), reporting through a
    /// [`TurnContext::background`] of `ctx`; `false` when it is not one.
    fn command(self: Arc<Self>, ctx: &TurnContext, name: &str, args: &str) -> bool {
        let _ = (ctx, name, args);
        false
    }

    /// Once, when the chat's engine starts: begin watching, and say the
    /// first status.
    fn opened(self: Arc<Self>, ctx: &TurnContext) {
        let _ = ctx;
    }

    /// Ctrl-C with no turn running: stop the commands running in the
    /// background, as Ctrl-C stops them in a terminal.
    fn interrupt(&self) {}

    /// The chat closes: stop the background commands and wait for them to
    /// wind down.
    fn shutdown(self: Arc<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}
