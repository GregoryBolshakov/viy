//! rpctl's DEFAULTS at the pinned commit, in its order: the lowest config
//! layer. The comments are rpctl's own and name rp's commands and modules.
//! `config/layering`'s `defaults_only` case checks keys, order and values.

/// Every key a config file can set, and its default.
pub const DEFAULTS: &[(&str, &str)] = &[
    ("MODEL_REPO", "huihui-ai/Huihui-Qwen3.8-27B-abliterated"),
    ("SERVED_MODEL_NAME", "qwen3.8-abliterated"),
    ("MAX_MODEL_LEN", "65536"),
    // Client-side per-reply cap. Counts the reasoning trace too, so a
    // thinking model needs real headroom or it truncates mid-thought.
    ("MAX_TOKENS", "16384"),
    // Client-side sampling. Qwen3.8's recommended thinking-mode values; a
    // lower temperature makes its reasoning loop on itself. TOP_K is sent only
    // to vLLM (it is not an OpenAI field).
    ("TEMPERATURE", "1.0"),
    ("TOP_P", "0.95"),
    ("TOP_K", "20"),
    // Client-side, vLLM only. Qwen3.8's chat template takes xhigh, medium or
    // low. xhigh adds a system line asking to "validate key assumptions,
    // consider plausible alternatives", and the model re-checked settled
    // points for dozens of paragraphs. Measured 2026-06-28 on 9 replayed
    // answer rounds: medium reasoned ~25% shorter (7 paragraphs vs 12) with
    // equally complete answers. On easy math medium is the longer one (2-3x
    // vs xhigh, 2026-06-26, same accuracy); /effort xhigh for that.
    ("REASONING_EFFORT", "medium"),
    // Client-side. History is resent whole every turn, so it grows until the
    // pod rejects the request; at this fraction of the usable budget the client
    // runs /compact for you first. Budget is max_model_len minus MAX_TOKENS,
    // since vLLM counts the reply allowance against the context too.
    ("AUTO_COMPACT", "1"),
    ("AUTO_COMPACT_THRESHOLD", "0.85"),
    // Client-side. 1 lets the model reason before writing a compaction
    // summary. Off by default: on a 29k-token chat reasoning doubled the pass
    // (489 s vs 242 s) and its output tokens, with the same recall (kb f099).
    ("COMPACT_THINKING", "0"),
    // Client-side. Compaction first just elides tool output older than the
    // last turn, no model call, when that brings the history under this
    // fraction of the budget (tool output is 81-93% of real chats, kb f101).
    ("COMPACT_TRIM_TARGET", "0.5"),
    // Client-side. After a turn that leaves the history above this fraction,
    // the summary a compaction will need is written in the background, so it
    // is ready when the history reaches AUTO_COMPACT_THRESHOLD. 0 disables.
    ("COMPACT_BACKGROUND_AT", "0.7"),
    // Client-side. Each summary pass summarizes only what followed the last
    // summary and appends it; once the summaries exceed this fraction of the
    // budget (or half of MAX_TOKENS), the background pass rewrites them as one.
    ("COMPACT_CONSOLIDATE_AT", "0.25"),
    // Client-side. At this fraction of the budget, tool output and write_file
    // contents from turns before the last TOOL_ELIDE_KEEP_TURNS are replaced
    // by a short note, in one batch. Tool output is most of a chat's history,
    // so this usually postpones compaction a long way. 0 disables it.
    ("TOOL_ELIDE_THRESHOLD", "0.6"),
    ("TOOL_ELIDE_KEEP_TURNS", "2"),
    // rp's CHAT_AUTOSAVE is not kept: viy saves every chat, as Codex
    // records every session (kb d083).
    // Client-side. After a chat's first reply, ask the model for a 3-6 word
    // title in the background (one short request, thinking off). A title you
    // set with /rename is never replaced. 0 keeps the first-message title.
    ("CHAT_AUTO_TITLE", "1"),
    // Client-side. rp chat animates the terminal tab's title while a turn is
    // in progress (model thinking or writing, tools running) and shows a
    // static mark when it waits for you. 0 leaves the title alone.
    ("CHAT_TAB_TITLE", "1"),
    // Client-side, viy only. viy chat draws the centipede header at the top
    // of the transcript (it scrolls away with it), unless NO_COLOR is set:
    // 1 its ghost (faint Braille dots in the terminal's own colours, no
    // background), colour the coloured art, 0 (or anything else) none.
    ("CHAT_HEADER", "1"),
    // Client-side. 1 masks API tokens (known key formats, and the values of
    // this config's own secrets) before a chat is written to disk. Off by
    // default because it is lossy: a resumed chat sees the mask, not the token.
    ("CHAT_REDACT", "0"),
    ("GPU_MEMORY_UTILIZATION", "0.90"),
    ("MAX_IMAGES_PER_PROMPT", "4"),
    ("MAX_NUM_SEQS", "64"),
    ("KV_CACHE_DTYPE", ""),
    ("MTP_SPECULATIVE_TOKENS", ""),
    ("STARTUP_CACHE", "1"),
    ("MODEL_CACHE_MODE", "auto"),
    ("SAFETENSORS_LOAD_STRATEGY", "prefetch"),
    // Read the cached weights into RAM from the first second of the boot, in
    // the background, while vLLM imports. Saves up to ~60s; 0 turns it off.
    ("WEIGHTS_PREWARM", "1"),
    // vLLM's startup warmup of the image processor: one maximum-size dummy
    // image, 16.5s of every boot. Off: images work the same, only the first
    // image request does that work instead. 1 restores vLLM's default.
    ("MM_WARMUP", "0"),
    // vLLM prefix caching, so a growing chat re-prefills only what is new. It
    // is experimental for this hybrid model; 0 turns it off. Changes the
    // compile hash, so the first boot per GPU type recompiles (~72s) once.
    ("PREFIX_CACHING", "1"),
    ("TRUST_REMOTE_CODE", "0"),
    ("DATA_CENTER_ID", "EUR-IS-1"),
    ("NETWORK_VOLUME_NAME", "qwen38-weights"),
    ("NETWORK_VOLUME_SIZE_GB", "100"),
    ("NETWORK_VOLUME_ID", ""),
    // When DATA_CENTER_ID has no GPU free, rp up offers the backup volume
    // (<NETWORK_VOLUME_NAME>-backup) in a second region, and to move it to where
    // RunPod shows an A100 free when that region is full too (rpctl/regions.py).
    ("FAILOVER", "1"),
    // When every region offered refuses the pod (RunPod's stock overstates
    // what it will place), keep retrying them, the primary and the old backup
    // for this many minutes; the first one to take the pod wins. 0 = one try.
    ("FAILOVER_RETRY_MIN", "15"),
    // rp reap deletes a backup volume no pod has used for this long; 0 keeps it.
    ("BACKUP_IDLE_DAYS", "30"),
    // Tried in order, so this ordering is the cost policy: cheapest first.
    // Every entry fits the 55.6GB of bf16 weights on one card.
    (
        "GPU_TYPE_IDS",
        "NVIDIA A100 80GB PCIe,NVIDIA A100-SXM4-80GB,NVIDIA RTX PRO 6000 Blackwell Server Edition,NVIDIA H100 NVL,NVIDIA H100 80GB HBM3,NVIDIA H200,NVIDIA B200",
    ),
    ("GPU_TYPE_PRIORITY", "custom"),
    ("GPU_COUNT", "1"),
    ("CLOUD_TYPE", "SECURE"),
    ("CONTAINER_DISK_GB", "60"),
    ("POD_NAME", "qwen38-abliterated"),
    // The stock Docker Hub image. RunPod hosts pull it in ~1-2 min; the
    // ghcr.io sshd variant (docker/Dockerfile) shares every base layer, but a
    // host without them pulls all 9.1GB from ghcr.io, measured at 10+ minutes
    // with retries (2026-06-25). That costs far more than the ~20s apt install
    // of sshd it saves, so it is opt-in only.
    ("VLLM_IMAGE", "vllm/vllm-openai:v0.27.1"),
    // CUDA versions a host's driver must support, as RunPod lists them
    // (allowedCudaVersions; its newest value is 13.0). The image above needs
    // cuda>=13.0 (NVIDIA_REQUIRE_CUDA in its config). A 12.8 host crash-looped
    // with "NVIDIA driver on your system is too old" on 2026-06-26. Change it
    // together with VLLM_IMAGE. Empty = any host. RunPod matches this list
    // exactly and its REST API accepts nothing above 13.0, so 13.1/13.2 hosts
    // are excluded; when that alone refuses a pod (GraphQL shows free GPUs only
    // on drivers >= the lowest value here), rp up asks again without it.
    ("CUDA_VERSIONS", "13.0"),
    ("VLLM_PORT", "8000"),
    // --- transport --------------------------------------------------------
    // 1 = reach vLLM only through an SSH tunnel to the pod's direct TCP port.
    // RunPod's https://<pod>-<port>.proxy.runpod.net endpoints terminate TLS at
    // their edge, so every prompt, completion and bearer token crosses their
    // network as plaintext. With TUNNEL=1 no http port is exposed at all, vLLM
    // binds to loopback in the container, and the encryption ends inside the
    // container instead of at their proxy. Costs a `rp tunnel` running
    // alongside. 0 restores the old direct-proxy behaviour.
    //
    // This hides the traffic from the proxy, NOT from RunPod: they have root on
    // the host, and inference needs your plaintext in the pod's RAM and VRAM.
    // See "Transport security" in the README.
    ("TUNNEL", "1"),
    // Local end of the forward. Empty = the same number as VLLM_PORT.
    ("TUNNEL_LOCAL_PORT", ""),
    // --- leak protection ----------------------------------------------------
    // The pod terminates itself after IDLE_TTL_MIN minutes in which vLLM took
    // in and produced no tokens (streaming counts as use; an open, idle chat
    // does not), and after MAX_POD_HOURS whatever happens. Fixed at pod
    // creation. 0 disables either. `rp reap` (run by a systemd timer, see
    // `rp reap --install`) is the laptop-side backup: it runs rp down once
    // this machine has streamed nothing for IDLE_TTL_MIN + REAP_GRACE_MIN.
    //
    // For that the pod uses the pod-scoped key RunPod injects into every pod
    // (RUNPOD_API_KEY inside the pod): it can terminate its own pod and nothing
    // else, verified 2026-06-26. RUNPOD_WATCHDOG_API_KEY overrides it with a
    // key of your own; leave it empty unless the injected key stops working.
    // It is passed in the pod env only, never written to the volume or logs.
    ("RUNPOD_WATCHDOG_API_KEY", ""),
    ("IDLE_TTL_MIN", "60"),
    ("MAX_POD_HOURS", "8"),
    ("REAP_GRACE_MIN", "15"),
    ("HF_TOKEN", ""),
    ("VLLM_API_KEY", ""),
    ("RUNPOD_API_KEY", ""),
    // Name of the active model profile in profiles/<name>.env, when you keep
    // more than one model around. Empty = just use the keys above. This is the
    // seam a future model swap hangs off (see `rp` docs).
    ("MODEL_PROFILE", ""),
    // Which backend hosts the model:
    //   runpod  vLLM on a RunPod GPU pod, managed by rp up/stop/down
    //   openai  a hosted OpenAI-compatible endpoint (ENDPOINT_* below); no pod
    //   local   vLLM on this machine, started by rp up and stopped by rp down
    ("BACKEND", "runpod"),
    // BACKEND=openai. ENDPOINT_URL may include or omit the trailing /v1.
    // ENDPOINT_MODEL empty means SERVED_MODEL_NAME.
    ("ENDPOINT_URL", ""),
    ("ENDPOINT_API_KEY", ""),
    ("ENDPOINT_MODEL", ""),
    // BACKEND=local. LOCAL_VLLM_PORT empty means VLLM_PORT.
    ("LOCAL_VLLM_BIN", "vllm"),
    ("LOCAL_VLLM_PORT", ""),
    // --- web search -------------------------------------------------------
    // The model's chat template emits <tool_call><function=..><parameter=..>,
    // which vLLM parses with qwen3_xml (alias of qwen3_coder in 0.27.1).
    ("TOOL_CALL_PARSER", "qwen3_xml"),
    ("SEARCH_ENABLED", "1"),
    ("TAVILY_API_KEY", ""),
    // Empty TAVILY_API_KEY: read this open (no-PIN) token from the tok vault.
    ("TAVILY_VAULT_TOKEN", "tavily-dev"),
    ("TAVILY_SEARCH_DEPTH", "basic"),
    ("SEARCH_MAX_RESULTS", "5"),
    ("SEARCH_MAX_CHARS_PER_RESULT", "1200"),
    ("SEARCH_MAX_ROUNDS", "4"),
    // fetch_url (rpctl/webfetch.py, rpctl/netguard.py): on with web search.
    // FETCH_MAX_CHARS is one part of a page sent to the model (~3.3k tokens);
    // FETCH_MAX_BYTES caps the decoded download, FETCH_TIMEOUT the whole read.
    ("FETCH_ENABLED", "1"),
    ("FETCH_MAX_CHARS", "12000"),
    ("FETCH_MAX_BYTES", "3000000"),
    ("FETCH_TIMEOUT", "30"),
    // --- instruction files ------------------------------------------------
    // Client-side. AGENTS.md / QWEN.md / CLAUDE.md found in the rpctl config
    // dir and in every directory from / down to the workspace are pasted into
    // the system prompt on every request (outermost first; later wins).
    // /instructions shows which; /instructions off disables for a session.
    ("INSTRUCTIONS_ENABLED", "1"),
    ("INSTRUCTION_FILES", "AGENTS.md,QWEN.md,CLAUDE.md"),
    // Total cap across all files; the project's own files are kept first.
    ("INSTRUCTIONS_MAX_CHARS", "32000"),
    // --- system prompt guidance (client-side) ------------------------------
    // 1 = the system prompt also carries the date (fixed at chat start, so the
    // prefix cache survives midnight), the OS and workspace, and rules for
    // judging what to do and how to answer (ANSWER_GUIDE / TOOL_WORK_GUIDE in
    // rpctl/chat.py). 0 = the tool-use guide only, as before 2026-07-03.
    ("SYSTEM_GUIDE", "1"),
    // The rule to use explore reports without re-reading their files (kb
    // f086), separately switchable because a no-re-verify line backfired once
    // (f088).
    ("SYSTEM_GUIDE_TRUST_EXPLORE", "1"),
    // --- build tools (write/read/list files + run shell) ------------------
    // On by default: an agent that can only talk about files is not much use.
    // run_shell runs every command in a bubblewrap sandbox on THIS machine
    // (no network, no home directory, the client's code and your .git
    // read-only; see SANDBOX_GIT)
    // without asking; only destructive commands and risky network requests
    // ask (rpctl/approve.py). /tools off to disable for a session, or set
    // TOOLS_ENABLED=0 in .env to default it off.
    ("TOOLS_ENABLED", "1"),
    // The mode rp chat starts in (rpctl/modes.py; Shift+Tab cycles them):
    //   normal    every question waits for your answer
    //   auto-yes  says yes to destructive commands, env-file reads and
    //             ask_claude; internet-facing questions and writes to the
    //             client's own files still ask
    // A project's .rpctl/config cannot set it.
    ("CHAT_MODE", "normal"),
    // Where sandboxed network commands can connect (netaccess Stage 6):
    //   proxy  their own network namespace whose only way out is the
    //          client's egress proxy (rpctl/egress.py): HTTP(S) to package
    //          registries, hosts written in the command and hosts you allowed
    //          at a fetch_url prompt; everything else is blocked and reported
    //   host   share this machine's network (bwrap --share-net): reaches your
    //          machine's services and the LAN. For tools that ignore proxies;
    //          warns on every network command
    ("SANDBOX_NET", "proxy"),
    // 1: in a workspace with a .git directory, the sandbox's .git is the
    // model's own repository (rpctl/sandboxgit.py): writable, local only,
    // encrypted with gocryptfs under ~/.local/share/rpctl/sandbox-git, its
    // key the tok token rpctl-sandbox-git (/git init). Yours stays read-only;
    // /git import brings its branches in as sandbox/*. 0: .git read-only.
    ("SANDBOX_GIT", "1"),
    // Extra hosts every network command may reach, comma-separated (a private
    // registry). *.example.com allows its subdomains; nothing else is a pattern.
    ("SANDBOX_NET_ALLOW", ""),
    // Where the model's files land and shell commands run. Empty means the
    // whole project (the directory you launched rp in) -- the agent works on
    // your real files, like a coding agent. A relative path scopes it to a
    // subfolder of the project; an absolute path is used as given.
    ("WORKSPACE_DIR", ""),
    ("TOOLS_SHELL_TIMEOUT", "120"),
    // Cap on the characters of file/command output fed back to the model (and
    // shown), so one noisy command can't blow the context window.
    ("TOOLS_MAX_OUTPUT", "20000"),
    // A build loop (write -> run -> read error -> fix) needs more iterations
    // than a search does. Used when tools are on.
    // 25: codebase questions used 7-13 rounds at xhigh (kb d009).
    ("TOOLS_MAX_ROUNDS", "25"),
    // Parallel work and tracing (rpctl/workflows.py, rpctl/trace.py).
    ("TOOLS_PARALLEL", "1"),
    ("EXPLORE_ENABLED", "1"),
    ("EXPLORE_MAX_TASKS", "4"),
    ("EXPLORE_PARALLEL", ""), // empty: MAX_NUM_SEQS - 1
    ("EXPLORE_MAX_ROUNDS", "10"),
    // ask_claude (rpctl/consult.py, docs/claude-consult-plan.md): Qwen hands a
    // task to the full Claude Code agent, which works in the workspace (act)
    // or only answers (advise). Runs on this machine; never on the boot path.
    ("CONSULT_ENABLED", "1"),
    ("CONSULT_MODE", "act"),
    ("CONSULT_MODEL", "claude-opus-5-5"),
    ("CONSULT_EFFORT", "xhigh"),
    ("CONSULT_PERMISSION_MODE", "bypassPermissions"),
    ("CONSULT_APPROVAL", "ask"), // ask: y/N per consult; auto: never ask
    ("CONSULT_TIMEOUT", "1800"),
    ("CONSULT_MAX_BUDGET_USD", ""), // empty: no cap
    ("CONSULT_REPORT_CHARS", "20000"),
    ("CONSULT_KEEP_DAYS", "7"),
    ("CONSULT_ROOT", ""), // empty: /tmp/rpctl-consult-<uid>
    ("CONSULT_CLAUDE_BIN", "claude"),
    // The toolbox (rpctl/toolbox.py, docs/toolbox-plan.md): command-line
    // tools kept between chats, indexed in the system prompt (frozen per
    // chat, so the prefix cache holds) and on run_shell's PATH.
    ("TOOLBOX_ENABLED", "1"),
    ("TOOLBOX_INDEX_MAX", "40"), // index lines; find_tool reaches the rest
    ("WRITE_REQUIRES_READ", "1"),
    ("TRACE_ENABLED", "1"),
];
