//! Where the model is hosted, from the `BACKEND` key: rpctl's
//! `resolve_backend`. So far only which one it is; the `openai` and
//! `local` backends themselves come with WP 2.10.

use viy_config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// vLLM on a RunPod GPU pod, the default.
    Runpod,
    /// A hosted OpenAI-compatible endpoint: nothing to bring up or down.
    Openai,
    /// vLLM on this machine.
    Local,
}

impl Backend {
    /// The one `BACKEND` names, in any case, empty meaning RunPod; else
    /// rpctl's message.
    pub fn resolve(cfg: &Config) -> Result<Self, String> {
        let kind = cfg.get("BACKEND").unwrap_or("").trim().to_lowercase();
        match kind.as_str() {
            "" | "runpod" => Ok(Self::Runpod),
            "openai" => Ok(Self::Openai),
            "local" => Ok(Self::Local),
            _ => Err(format!(
                "unknown BACKEND '{kind}'; supported: local, openai, runpod"
            )),
        }
    }

    /// Its `BACKEND` value.
    pub fn name(self) -> &'static str {
        match self {
            Self::Runpod => "runpod",
            Self::Openai => "openai",
            Self::Local => "local",
        }
    }

    /// Whether it has a server for `viy down` to take down.
    pub fn has_down(self) -> bool {
        self != Self::Openai
    }
}
