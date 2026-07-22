use crate::Answer;
use crate::Event;
use crate::Question;

/// Where the engine's events go and who answers its questions: the terminal
/// UI, the plain renderer, `viy dev drive`, or a test's scripted frontend.
/// The one seam that replaces rpctl tests' patching of `print`, `input`,
/// `_confirm` and `isatty` (kb d037).
pub trait Frontend: Send + Sync {
    /// Shows one event. Never waits for the user.
    fn emit(&self, event: Event);

    /// Asks `question` and waits for the answer. A frontend with nobody to
    /// ask returns `question.unattended`, never a guess. Replaced in WP 3.1
    /// by a request event answered by an `Op`, as in Codex (kb d036).
    fn ask(&self, question: &Question) -> Answer;
}
