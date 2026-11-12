//! Configuration: the tool, global and project scopes, the `.viy/` marker
//! (`.rpctl/` still recognised), layering from DEFAULTS through the global
//! file, the model profile, the project file and the environment, model
//! profiles, `viy init` and `viy migrate`, open tokens from the tok vault, and
//! redaction. Ported from rpctl's `config.py`, `vault.py` and `redact.py`
//! (ROADMAP phase 1).

pub mod atomic;
pub mod config;
pub mod cutover;
mod defaults;
pub mod migrate;
pub mod paths;
pub mod profiles;
pub mod project;
pub mod redact;
pub mod vault;

pub use config::Config;
pub use config::Layers;
pub use defaults::DEFAULTS;
pub use paths::Paths;
pub use project::Project;
