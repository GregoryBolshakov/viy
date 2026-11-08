---
id: kbs-schema
subject: kb-system
relation: frontmatter_contract
object: "flat key:value frontmatter, SRO key + bi-temporal fields + review_by TTL"
status: current
valid_at: {today}
recorded_at: {today}
invalidated_at: null
supersedes: null
superseded_by: null
confidence: high
source: kb-system/schema.md
source_type: self
review_by: {ttl_180}
---

# Frontmatter schema (the contract)

Every file under `kb/` carries this block. `tools/kb_lint.py` enforces it.
Flat `key: value` only - no nested YAML, so the parser stays dependency-free.

```yaml
---
id: f001-example                     # stable slug, never reused, never renamed
subject: acme                        # the entity the claim is about
relation: list_price_starter_monthly # the predicate - single-valued, scope in the name
object: "49 USD/month"               # the value - canonical form, compared as a string
status: current                      # current | superseded | disputed | retired
valid_at: {today}                  # when this became true in the world
recorded_at: {today}               # when we learned it (transaction time)
invalidated_at: null                 # set when contradicted; never delete the file
supersedes: null                     # id of the fact this replaces
superseded_by: null                  # id of the fact that replaced this
confidence: high                     # high | medium | low
source: https://acme.com/pricing     # the URL or origin
source_type: primary                 # primary | vendor-marketing | community | inference | self
review_by: {ttl_90}                # explicit TTL - the freshness deadline
---
```

## Field notes

**`subject` + `relation` are the conflict key.** Two files with `status: current`
sharing the same `(subject, relation)` but different `object` is a contradiction, and
the linter fails the build. That single rule is the whole mechanism.

**`object` is compared as an exact string.** The same value must always be spelled the
same way: `49 USD/month`, `131072 tokens`, ISO dates, `true`/`false`, lowercase slugs. One
value, no hedging - uncertainty goes in `confidence`, detail in the body. A sentence here
turns every restatement into a CONTRADICTION.

**Reuse key spellings.** `list-price` next to `list_price` is two keys, and the conflict
check never compares them. Run `python3 tools/kb_facts.py keys` before inventing one, and
put scope into the relation (`price_starter_monthly`, `price_pro_monthly`) because each
`(subject, relation)` can hold only one live value.

**`valid_at` vs `recorded_at` is bi-temporal.** Valid time is when the fact was true in
the world; transaction time is when this KB learned it. You need both to answer "what did
I believe in June, and was I wrong?" - the question that matters when a plan fails and
you are auditing why.

**`invalidated_at` + `superseded_by` replace deletion.** Never delete a fact. Retire it.
The history is the audit trail, and a deleted fact is one you will re-derive and re-trust
six months from now.

**`review_by` is the load-bearing field.** It is a deadline, not a guess. When it passes,
the linter errors. A KB without TTLs decays silently; a KB with them decays loudly.

| Class of claim | TTL | Why |
|---|---|---|
| Pricing, fee splits, rate cards | 90 days | Changes without announcement |
| Competitor coverage, user counts, rankings | 90 days | Moves continuously |
| Protocol / spec behaviour | 180 days | Versioned, slower |
| Structural market claims | 180 days | Trend-level |
| My own decisions and strategy | 120 days | Should be revisited on purpose |

**`source_type` gates `confidence`.** `vendor-marketing` and `inference` cannot be
`high` confidence - the linter warns. Most of what is published about any young market is
content marketing by someone selling an adjacent product, so this field is not
bureaucratic; it is the main defence against building on a number somebody invented.

**`source_type: self`** means the claim originates here - a decision, or a metric you
measured yourself. Those are the highest-value facts in the KB, because they are the only
ones nobody else has.

## Statuses

| status | meaning |
|---|---|
| `current` | live, trusted, counted in contradiction checks |
| `superseded` | replaced; must name its `superseded_by` |
| `disputed` | two sources conflict and neither has won yet |
| `retired` | no longer relevant, but kept for the record |
