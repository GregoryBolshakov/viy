# {project}

{context}

Start at [`kb/00-index.md`](kb/00-index.md).

## Layout

```
{guide_line}
kb/
  00-index.md      entry point - read this first
  strategy/        what we're doing and why       (s0**)
  facts/           atomic, supersede-able claims  (f0**)
  decisions/       ADRs                           (d0**)
kb-system/
  schema.md        the frontmatter contract
tools/
  kb_lint.py       deterministic freshness + contradiction checker
  kb_facts.py      key-spelling + wording checker (what kb_lint can't see)
.githooks/
  pre-commit       runs both; blocks commits that break the contract
  kb_lint.py       the hook's own copies of the checkers, which agents
  kb_facts.py      cannot write; the hook stops if tools/ differs
.viy/              viy project marker: pins the agent's workspace here
```

## Working on it with viy

```bash
viy up     # if the pod isn't running
viy chat   # from anywhere inside this folder
```

`viy chat` loads `{guide_file}` into the system prompt on every turn and confines the
agent's file tools to this folder. The model is told to run the linter and read
the index before anything else, so the first turn costs a couple of tool calls.

viy runs a `run_shell` command that needs no network in its sandbox without
asking, so the linter runs as soon as the model calls it; it needs no network or
home dir.

## Using the linter

```bash
python3 tools/kb_lint.py           # human output, exit 1 on error
python3 tools/kb_lint.py --json    # machine output
python3 tools/kb_lint.py --quiet   # errors only
python3 tools/kb_lint.py --today 2027-01-01   # what goes stale by then
```

The linter is the point. It checks, deterministically and with no model involved:

- every required frontmatter field is present and well-formed
- no `review_by` TTL has passed on a `current` document (**STALE**)
- no two `current` docs share a `(subject, relation)` with different `object`
  (**CONTRADICTION**)
- every supersession chain is symmetric and complete
- `confidence: high` is not claimed on a marketing or inferred source
- every `[[wikilink]]` resolves to a real id

`tools/kb_facts.py` covers what that can't: the same key spelled two ways (never
compared), the same value spelled two ways (a false CONTRADICTION), relative time
("currently", "latest"), hedged or paragraph-long `object` values.

```bash
python3 tools/kb_facts.py keys            # every (subject, relation) in use
python3 tools/kb_facts.py check           # HARD + SOFT findings
python3 tools/kb_facts.py check --strict  # exit 1 on HARD (what the hook runs)
```

`viy init --kb` points git at `.githooks/`, so both run on every commit. If you cloned
this KB elsewhere, run `git config core.hooksPath .githooks` once.

## The rule that matters

Never edit a fact's `object` in place; never delete a fact file. Supersede it. The
history is what tells you when you were wrong and what you did while you believed it.
