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
    /// A question for the user, answered by [`crate::Op::QuestionAnswer`]
    /// with its `question_id` (kb d036).
    QuestionRequest(QuestionRequestEvent),
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
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodStatusEvent {
    /// `runpod`, `openai` or `local`.
    pub backend: String,
    pub gpu: Option<String>,
    pub price_usd_per_hour: Option<f64>,
    pub idle_seconds_left: Option<u64>,
    pub tunnel_up: Option<bool>,
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
pub struct HistoryRewrittenEvent {
    pub reason: RewriteReason,
    pub items: Vec<HistoryItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RewriteReason {
    Rewind,
    Compaction,
    New,
    Resume,
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
}
