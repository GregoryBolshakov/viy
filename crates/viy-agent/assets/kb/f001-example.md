---
id: f001-example
subject: example-vendor
relation: list_price_starter_monthly
object: "49 USD/month"
status: current
valid_at: {today}
recorded_at: {today}
invalidated_at: null
supersedes: null
superseded_by: null
confidence: low
source: https://example.com/pricing
source_type: vendor-marketing
review_by: {ttl_90}
---

# Delete me

## Claim
Example Vendor's Starter plan costs 49 USD per month when billed monthly.

## Evidence
- "Starter - $49 / month" (https://example.com/pricing, retrieved {today})

## Method
Read the public pricing page once; n=1.

## Scope
Monthly billing only; excludes tax.

## About this file

This file exists so the linter has something to chew on and so you can see the shape of
a fact before writing a real one. Copy it, then delete it.

Note what it demonstrates:

- `confidence: low` on `source_type: vendor-marketing`. Setting this to `high` makes the
  linter warn, because a vendor's own page is not evidence about that vendor.
- A 90-day `review_by`, because pricing moves without announcement.
- `object` is one canonical value (`49 USD/month`, not `$49/mo for the starter tier`),
  because it is compared as a string. The scope ("starter", "monthly") lives in the
  `relation`; the nuance lives in the body.
- The body is Claim / Evidence / Method / Scope: what a future agent needs to re-check it.
  Evidence is quoted verbatim with a retrieval date.

To supersede this fact rather than edit it:

1. Write `f002-whatever.md` with `supersedes: f001-example`.
2. Set this file to `status: superseded`, `invalidated_at: <new fact's valid_at>`,
   `superseded_by: f002-whatever`.
3. Re-run the linter. It fails the chain if you half-do it.
