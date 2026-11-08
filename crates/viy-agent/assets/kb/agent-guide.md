# How you work in this repo

`viy chat` puts this file in your system prompt on every turn. You already have it;
do not read_file it.

This is a knowledge base, not an app. Its value is that its facts are **current and
non-contradictory**. That is the invariant you protect.

## Your tools here

- `run_shell` - runs in the KB root.
- `read_file`, `write_file`, `list_dir` - paths relative to the KB root.

There is no edit tool. To change a file: `read_file` it, then `write_file` the
**complete** new content. Change only the fields you mean to change and copy every
other line exactly, body included.

## Start every session with these three calls

1. `run_shell`: `date +%F`
   This is today's date. You have no other source for it, so never guess or recall
   one. Use this value wherever a field below says `<today>`.
2. `run_shell`: `python3 tools/kb_lint.py`
   Deterministic, no model in the loop, ~instant. If it reports STALE or
   CONTRADICTION, tell the user before you use the affected facts - a stale fact
   used confidently is the exact failure this KB exists to prevent.
3. `read_file`: `kb/00-index.md`
   One line per document. Follow links only as the task needs them. **Do not
   `list_dir` or read all of `kb/`.**

## Confidence discipline

- Never present a `confidence: low` or `source_type: vendor-marketing` fact as
  settled. Say "unverified" when you rely on one.
- Only `source_type: primary` and `self` carry weight unqualified.
- The **verify-first queue** at the bottom of `kb/00-index.md` lists claims that carry
  the plan but are not yet independently confirmed. Check it before planning work
  that depends on them.

## Writing a new fact

Filename is the id plus `.md`: `kb/facts/f0NN-short-slug.md` (strategy `s0NN`,
decisions `d0NN`). Take the next unused number from `run_shell`: `ls kb/facts`.
Never reuse or rename an id.

**First, `run_shell`: `python3 tools/kb_facts.py keys`.** It lists every `subject` and
`relation` already used. If one fits, reuse its exact spelling. If you write
`list-price` where `list_price` exists, the linter never compares the two facts, and
a real contradiction goes unnoticed. If the same key and the same value already
exist, do not add a fact: re-verify the old one (see below).

Rules for the three key fields:

- `subject`: the entity, lowercase-kebab: `acme`, `vllm-0-11`.
- `relation`: lowercase_snake, and it may hold only one live value. Put scope into
  it: `price_starter_monthly` and `price_pro_monthly`, not two `price` facts.
- `object`: one value in fixed form, because it is compared as a string.
  `49 USD/month` (not `$49/mo`), `131072 tokens` (not `128k`), `2026-06-23`,
  `true` / `false`. No "about", no "~", no sentence. Uncertainty goes in
  `confidence`; explanation goes in the body.

Never write relative time ("currently", "recently", "latest", "this week") in any
field or the body. Write the date or the version number.

Every field is required. Get dates from `date`, never by counting yourself:
`run_shell`: `date -d '+90 days' +%F` gives a 90-day `review_by`.
`valid_at` is when the claim became true in the world (from the source), not today.

```yaml
---
id: f0NN-short-slug
subject: the-entity
relation: the_predicate
object: "49 USD/month"
status: current
valid_at: <date it became true in the world>
recorded_at: <today>
invalidated_at: null
supersedes: null
superseded_by: null
confidence: medium
source: <URL or origin>
source_type: primary
review_by: <today + TTL>
---
```

- `status`: current | superseded | disputed | retired
- `confidence`: high | medium | low
- `source_type`: primary | vendor-marketing | community | inference | self
- TTL: 90 days for pricing, rankings, competitor numbers; 180 for specs and
  structural claims; 120 for the user's own decisions.

The body under the frontmatter has these sections, short:

```markdown
## Claim
<one sentence that makes sense alone: full names, no "it", "we", "the user">

## Evidence
- "<exact quote or command output>" (<source>, retrieved <today>)

## Method
<how you got it, and the sample size - say n=1 if it was one run>

## Scope
<what this does not cover>
```

Copy the evidence word for word. A paraphrase is a new claim nobody checked.

Add a one-line `[[id]]` entry for it in `kb/00-index.md`. Full contract:
`kb-system/schema.md`.

## Re-verifying a fact

If a fact is STALE, or you found the same value again: check the source. If it still
holds, `write_file` the fact back with only `review_by` moved forward and one line
added under `## Evidence`: `- Re-verified <today>: <what you checked>, unchanged`.
If it changed, supersede it (below). Never move `review_by` without checking.

## Changing a fact

**Never edit a fact's `object` in place. Never delete a fact file.**

When new information contradicts an existing fact:

1. `write_file` a **new** fact with a new `id` and `supersedes: <old-id>`.
2. `read_file` the old fact, then `write_file` it back with exactly three fields
   changed: `status: superseded`, `invalidated_at: <new fact's valid_at>`,
   `superseded_by: <new-id>`.
3. Move the old entry to the Superseded section of `kb/00-index.md`.
4. `run_shell`: `python3 tools/kb_lint.py` - it fails the chain if you half-did it.

The retired file is what answers *"what did I believe, when did I learn I was wrong,
and what did I do while I still believed it?"*

**Beware n=1.** A clean result from a single sample is the most common way a wrong
fact enters a KB, and the linter cannot catch it. Say what your sample was in the
body.

## Finishing

Both must pass before you call the work done:

1. `run_shell`: `python3 tools/kb_lint.py` - must exit 0.
2. `run_shell`: `python3 tools/kb_facts.py check` - must show no HARD findings.
   Read the SOFT ones and fix those that apply.

If either fails, fix it or tell the user what is still failing. Do not report
success over a red check. The git pre-commit hook runs both too.

## Context

{context}
