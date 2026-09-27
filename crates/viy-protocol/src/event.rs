use serde::Deserialize;
use serde::Serialize;

use crate::Question;

/// One event from the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The id of the [`crate::Submission`] this event belongs to.
    pub id: String,
    pub msg: EventMsg,
}

/// The events of docs/ui-plan.md 8. Every message rp chat prints maps to one
/// of them (docs/ui-coverage.md, WP 3.9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventMsg {
    TurnStarted(TurnStartedEvent),
    TurnComplete(TurnCompleteEvent),
    /// The turn stopped before it ended on its own (Ctrl-C).
    TurnAborted(TurnAbortedEvent),
    /// One model request within a turn.
    RoundStarted(RoundStartedEvent),
    ReasoningContentDelta(DeltaEvent),
    AgentMessageContentDelta(DeltaEvent),
    ToolCallBegin(ToolCallBeginEvent),
    ToolCallOutputDelta(ToolCallOutputDeltaEvent),
    ToolCallEnd(ToolCallEndEvent),
    Notice(NoticeEvent),
    Status(StatusEvent),
    /// How full the context is.
    TokenCount(TokenCountEvent),
    ModeChanged(ModeChangedEvent),
    EffortChanged(EffortChangedEvent),
    TitleChanged(TitleChangedEvent),
    PodStatus(PodStatusEvent),
    Background(BackgroundEvent),
    /// The history on screen is no longer what was shown: draw `items`.
    HistoryRewritten(HistoryRewrittenEvent),
    /// The history was compacted: the transcript stays and gets a marker
    /// (kb d053).
    ContextCompacted(ContextCompactedEvent),
    /// Text for the input box, to edit and send (a rewound message).
    InputPrefill(InputPrefillEvent),
    /// A question for the user, answered by [`crate::Op::QuestionAnswer`]
    /// with its `question_id` (kb d036).
    QuestionRequest(QuestionRequestEvent),
    /// The engine no longer waits for the answer to a question it asked:
    /// the work that asked it stopped (Codex's `ServerRequestResolved`).
    /// Not sent for a question answered by [`crate::Op::QuestionAnswer`].
    QuestionResolved(QuestionResolvedEvent),
    /// A slash command has run.
    CommandComplete(CommandCompleteEvent),
    /// The engine has stopped; nothing follows.
    ShutdownComplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRequestEvent {
    pub question_id: String,
    pub question: Question,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionResolvedEvent {
    pub question_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCompleteEvent {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    pub turn_id: String,
    pub user_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCompleteEvent {
    pub turn_id: String,
    pub outcome: TurnOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnOutcome {
    Answered,
    /// The round cap was reached and the capped final request answered.
    Capped,
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAbortedEvent {
    pub turn_id: String,
    pub reason: TurnAbortReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnAbortReason {
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundStartedEvent {
    /// 1 for the turn's first request.
    pub round: u32,
    pub kind: RoundKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RoundKind {
    Turn,
    /// The request without tools after the round cap.
    CappedFinal,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaEvent {
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallBeginEvent {
    pub call_id: String,
    pub tool: String,
    /// The arguments as the transcript shows them.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallOutputDeltaEvent {
    pub call_id: String,
    pub chunk: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEndEvent {
    pub call_id: String,
    pub status: ToolCallStatus,
    /// The result in a few words, as rp prints it after the call:
    /// `exit 1`, `timed out`, `1234 bytes`, `12 entries`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub duration_ms: u64,
    pub sandboxed: bool,
    pub network: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallStatus {
    Completed,
    Failed,
    /// Refused by a safeguard or answered no.
    Declined,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeEvent {
    pub level: NoticeLevel,
    /// Stable snake_case name for tests and the UI (`tunnel_down`,
    /// `length_cut`, `quarantined`, ...); the text may change, the code not.
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEvent {
    pub phase: StatusPhase,
    /// Tokens received in this round so far.
    pub tokens: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StatusPhase {
    /// Sent, nothing back yet.
    Waiting,
    Reasoning,
    Answering,
    RunningTools,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCountEvent {
    pub used: u64,
    pub budget: u64,
    /// Where auto-compaction starts, if it is on.
    pub auto_compact_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeChangedEvent {
    /// The mode's name (rpctl `modes.MODES`).
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortChangedEvent {
    pub effort: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleChangedEvent {
    /// The chat's name (rp's `_tab_label`): its title, else its first
    /// message; empty while it has neither (a new chat).
    pub title: String,
}

/// The model server as the footer shows it (ui-plan 3); each field the
/// sender does not know is `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodStatusEvent {
    /// `runpod`, `openai` or `local`.
    pub backend: String,
    /// The model's name, for a backend that is not the pod's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub state: PodState,
    pub gpu: Option<String>,
    pub price_usd_per_hour: Option<f64>,
    /// What the pod has cost while this chat was open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent_usd: Option<f64>,
    /// Until the reaper takes an idle pod down, counted when sent.
    pub idle_seconds_left: Option<u64>,
    pub tunnel_up: Option<bool>,
}

/// Where the model server is (kb d041): the chat opens in any of these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PodState {
    /// No pod, or a backend that needs none.
    #[default]
    Absent,
    /// `/up` is running; `stage` is the boot stage it reports.
    Booting {
        stage: String,
    },
    Ready,
    Stopping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundEvent {
    pub task: BackgroundTask,
    pub state: BackgroundState,
    pub progress: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackgroundTask {
    Title,
    Compaction,
    /// The explore workflow's helper number `n`.
    Helper {
        n: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundState {
    Started,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactedEvent {
    /// Messages in the history before and after.
    pub messages_before: u64,
    pub messages_after: u64,
    /// The prompt's estimated tokens before and after.
    pub tokens_before: u64,
    pub tokens_after: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputPrefillEvent {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRewrittenEvent {
    pub reason: RewriteReason,
    pub items: Vec<HistoryItem>,
}

/// Why the history was drawn again. A compaction is not one: the
/// transcript stays and [`EventMsg::ContextCompacted`] marks it (kb d053).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RewriteReason {
    Rewind,
    New,
    Resume,
    /// `/load`: a saved transcript replaced the history.
    Load,
}

/// One entry of the history as the UI draws it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum HistoryItem {
    User {
        text: String,
    },
    Agent {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        call_id: String,
        tool: String,
        summary: String,
    },
    Notice(NoticeEvent),
    /// The model reasoned before this reply; the trace is not kept.
    Thought,
    /// Above, the turns a compaction summarised; the model goes on from
    /// its summary (kb d052).
    Compacted,
}
