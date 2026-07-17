//! The safeguards around what the model does: command approval and the chat
//! modes, the bubblewrap sandbox with its seccomp filter, the egress proxy,
//! netguard, `fetch_url` and `web_search`, untrusted-text handling and
//! mounts. Ported from rpctl's `approve.py`, `modes.py`, `sandbox.py`,
//! `egress.py`, `netguard.py`, `webfetch.py`, `search.py`, `untrusted.py` and
//! `mounts.py` (ROADMAP phases 4 and 5).
