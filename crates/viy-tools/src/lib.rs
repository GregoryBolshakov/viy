//! The safeguards around what the model does: command approval and the chat
//! modes, the bubblewrap sandbox with its seccomp filter, the egress proxy,
//! netguard, `fetch_url` and `web_search`, untrusted-text handling and
//! mounts. Ported from rpctl's `approve.py`, `modes.py`, `sandbox.py`,
//! `egress.py`, `netguard.py`, `webfetch.py`, `search.py`, `untrusted.py` and
//! `mounts.py` (ROADMAP phases 4 and 5).
//!
//! So far the command approver and its network reviewer ([`approve`]), the
//! chat modes ([`modes`]), the bubblewrap sandbox and its seccomp filter
//! ([`sandbox`]), the quarantine of auto-running paths ([`quarantine`]),
//! secret file names ([`secrets`]), the path
//! resolution the guards share ([`resolve`]), and the schemas of the
//! internet tools ([`web`]), which the chat offers from phase 3 on.

pub mod approve;
pub mod guard;
pub mod modes;
pub mod mounts;
pub mod quarantine;
pub mod resolve;
pub mod sandbox;
pub mod secrets;
pub mod untrusted;
pub mod web;
