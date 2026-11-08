---
id: s001-pinned-calendar
subject: project
relation: validation_calendar
object: "day0 {day0}; day90 {day90}; stop {stop}"
status: current
valid_at: {day0}
recorded_at: {today}
invalidated_at: null
supersedes: null
superseded_by: null
confidence: high
source: self
source_type: self
review_by: {ttl_120}
---

# The dates, pinned

Written down so they cannot drift later, when moving them would be convenient.

| Marker | Date |
|---|---|
| Day 0 | **{day0}** |
| Day 30 | {day30} |
| Day 90 - first real read | **{day90}** |
| Six-month stop-loss | **{stop}** |

Record local dates, not UTC.

## What the markers mean

- **Day 30** is noise. Do not gate anything on it.
- **Day 90** is the first read with enough signal to act on. Decide what evidence would
  make you continue *before* you get there, and write it here.
- **The stop-loss is a commitment, not a suggestion.** If the plan has not cleared its
  bar by {stop}, it stops. Deciding that now, in writing, is the only time you can decide
  it honestly.

## The bar

_(Fill this in now, not at day 90.)_
