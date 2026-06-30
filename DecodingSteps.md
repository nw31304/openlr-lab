# OpenLR Decoding — Step-by-Step Reference for LLM Interpretation

This document describes how the OpenLRLens decoder works. It is intended to help an LLM
correctly interpret JSON trace data emitted during a decode attempt. Read it before
attempting to diagnose a decode result.

---

## Scoring conventions

- **Lower score = better match.** A score of 0 is a perfect match. All penalties add to
  the score; there are no bonuses.
- **Total score is additive:** `total = positional_distance + bearing_penalty + frc_penalty + fow_penalty`.
  Each term is independently attributable, which means a diagnosis can say "the gap came
  from bearing" or "from FRC" rather than just "it scored worse".
- **Hard gates vs. soft penalties:** some checks are hard gates — a candidate that fails
  them is **rejected outright** and will not appear in the trace at all (or will appear with
  a `rejected` flag and a reason). A candidate that passes all hard gates but scores poorly
  still appears and is **ranked by its total score**. When diagnosing, distinguish between
  "this candidate was never considered" (hard gate failure) and "this candidate was
  considered but outscored" (soft penalty).

---

## Format differences: TomTom v3 vs. TPEG-OLR

Two physical formats are supported. The format affects how intervals are interpreted
throughout the decode.

| Property | TomTom binary v3 | TPEG-OLR (ISO 21219-22) |
|---|---|---|
| Bearing precision | 5-bit, 32 sectors × 11.25° — each value represents a **bucket** `[LB, UB]` of width 11.25° | Full precision — `LB == UB` (a point, not a bucket) |
| DNP precision | 1-byte, ~58.6 m buckets — each value is a `[LB, UB]` interval | Exact — `LB == UB` |
| Offset precision | Same ~58.6 m buckets as DNP | Exact |

When you see a bearing or DNP as `[LB, UB]` in the trace, a v3 interval can be up to 11.25°
or ~58.6 m wide. For TPEG the interval is a point. A candidate sitting at the edge of a v3
interval may simply be at the edge of a quantisation bucket — this is **not necessarily a
poor match**; it is an artefact of v3's limited precision.

---

## Step 1 — Parse the OpenLR code

- Parse binary (v3) or XML/JSON (TPEG) into a `LocationReference` structure containing one
  or more Location Reference Points (LRPs).
- Each LRP carries: coordinate (lat/lon), FRC, FOW, bearing `[LB, UB]`, and — for all but
  the last LRP — DNP `[LB, UB]` and LFRCNP.
- If the input is syntactically invalid, abort immediately with a parse error.

---

## Step 2 — Candidate selection (one pass per LRP)

For each LRP, the decoder finds segments on the map that could plausibly represent that LRP.

### 2a — Spatial fetch

Fetch all segments within the configured `candidate_search_radius_m` of the LRP coordinate.
If no segments are found, abort and suggest:
- Increase the search radius
- The map may be missing segments in this area
- The wrong map version may be loaded

### 2b — Snap point determination

For each fetched segment, find where the LRP should snap to it:

1. **Project** the LRP coordinate onto the segment polyline to find the nearest interior
   point (perpendicular foot along the closest polyline edge).
2. **Check proximity to endpoints:** if the projected point falls within a small threshold
   distance of either endpoint, **shift the snap point to that endpoint**.
3. Otherwise, the snap point **remains in the interior** of the segment.

The distinction matters because:
- An interior snap means the decoder will conceptually split the segment at that point;
  A\* routing departs from (or arrives at) that interior position.
- An endpoint snap anchors routing to a known graph node, which is cleaner and matches
  the typical intent of an encoder that placed an LRP at a junction.

### 2c — Bearing computation

The bearing used for scoring is **computed at the snap point**, not at the segment endpoint.
Starting from the snap point, measure the heading along the segment geometry over the
**next 20 m** in the direction of travel (forward for non-terminal LRPs; backward for the
terminal LRP). This 20 m window bearing is what is compared against the LRP's bearing
interval `[LB, UB]`.

> **Why this matters for diagnosis:** a segment that looks wrong when measured from its
> start node may look correct when measured from a mid-segment snap point. If a trace shows
> an unexpected bearing score, check *where* on the segment the snap landed.

### 2d — Candidate scoring

Each candidate is scored against the LRP's attributes. **All terms are soft penalties
(lower is better) unless otherwise noted.**

| Term | Description |
|---|---|
| **Positional distance** | Straight-line distance (metres) from the LRP coordinate to the snap point. This is the **primary ranking term**. |
| **Bearing penalty** | Zero if the computed 20 m bearing falls inside the LRP's `[LB, UB]` interval (widened by `bearing_tolerance_deg` τ). Grows with distance outside the interval. |
| **FRC penalty** | Penalty for mismatch between the segment's FRC and the LRP's FRC. |
| **FOW penalty** | Penalty for mismatch between the segment's FOW and the LRP's FOW. |
| **Wrong-endpoint penalty** | Applied when the snap shifts to an endpoint that is the *wrong* end for the direction of travel. For a non-terminal LRP, the expected snap is near the **start** of the segment in the direction of travel; snapping to the far endpoint incurs this penalty. For the terminal LRP the logic inverts: expected near the **end** in the direction of travel. |

> **Note:** DNP (distance to next point) is **not** scored per candidate. It is validated
> after routing completes (Step 3). Do not expect to see DNP in per-candidate trace events.

### 2e — Hard gate: bearing window

A candidate whose computed bearing falls **outside** `[LB − τ, UB + τ]` is **rejected**
(hard gate). It will not appear among the accepted candidates. If too many candidates are
rejected on bearing, consider widening `bearing_tolerance_deg`.

### 2f — Retain top-N candidates

Keep the top-N candidates per LRP by total score. If no candidates survive, abort and
suggest:
- Relax bearing, FRC, or FOW tolerances
- The map may be missing or misclassified segments

---

## Step 3 — Route search

Find a connected path through the map that threads all LRP candidates in order.

### 3a — Candidate permutation order

The decoder iterates over all combinations of candidates (one per LRP), ordered by
**ascending combined score** — lowest (best) combined score first. This means the most
promising combination is tried first.

### 3b — A\* routing between adjacent LRP candidates

For each adjacent pair of selected candidates, run A\* to find the shortest-cost path:

- **State:** each A\* node is `(graph_node, incoming_segment)`, not just a bare node. This
  is required to honour turn restrictions — a prohibited `(from_segment, via_node, to_segment)`
  triple can only be detected if the incoming segment is tracked.
- **LFRCNP hard gate:** at each expansion, if the candidate edge's FRC is worse (higher
  number) than the leg's LFRCNP, that edge is **skipped entirely**. This is a hard gate on
  every individual expansion, not just a global threshold.
- **DNP termination:** if the cumulative route length exceeds `DNP_UB + δ` (the upper bound
  of the DNP interval plus the configured tolerance), the search is **terminated** for this
  candidate pair and the result is cached. This is separate from the expansion cap.
- **Expansion cap:** if the maximum number of node expansions is reached, the search is
  also terminated and cached.
- Results (success or failure) are **cached** per candidate pair, so the same pair is never
  routed twice across different permutations.

### 3c — Route outcome

- If a connected route is found across all LRP pairs: **success**, proceed to Step 4.
- If the candidate permutation is exhausted (or the `max_routing_attempts` cap fires):
  abort and suggest:
  - Increase LFRCNP tolerance (to allow the path to use lower-class roads)
  - Increase DNP tolerance
  - Increase `max_routing_attempts`
  - The map may be missing a connecting segment

---

## Step 4 — Offset trimming

The found route is a full path from the first LRP snap to the last LRP snap. Positive and
negative offsets trim it from each end to produce the final decoded location.

- **Positive offset** trims from the **path start** (first LRP end).
- **Negative offset** trims from the **path end** (last LRP end).
- Because v3 encodes offsets in ~58.6 m buckets, each offset is an interval `[LB, UB]`.
  TPEG offsets are exact (`LB == UB`).
- The decoder uses the **lower bound (LB)** of each offset interval, which gives the
  *minimum* trim — the conservative choice that maximises the reported location length.
- A sanity check ensures that the remaining path length `LB(pos)` to `LB(neg)` is
  non-negative. If it is negative, the offsets over-trim and a diagnostic is reported.
- The **uncertainty** in the final location boundary is the width of the offset intervals
  (`UB − LB` at each end).

---

## Trace event structure

*(This section will be expanded as the trace event schema is finalised.)*

Trace events are emitted as a JSON array. Each event has at minimum a `type` field
identifying the event kind, and additional fields specific to that type. Key event types
include:

| Event type | Emitted when |
|---|---|
| `candidate_scored` | A segment has been projected, snapped, and scored for a given LRP |
| `candidate_rejected` | A segment failed a hard gate and was not accepted |
| `astar_expand` | A\* expands a node during route search |
| `astar_skip_frc` | A\* skips an edge due to LFRCNP hard gate |
| `astar_skip_turn` | A\* skips an edge due to a turn restriction |
| `astar_skip_direction` | A\* skips an edge due to one-way direction constraint |
| `route_found` | A connected route was found for a candidate combination |
| `route_failed` | No route found for a candidate combination |
| `route_attempts_exhausted` | The `max_routing_attempts` cap fired before a route was found |
| `dnp_exceeded` | A\* terminated because cumulative length exceeded DNP upper bound + tolerance |
| `decode_success` | Full decode succeeded; includes the final path and offset-trimmed location |
| `decode_failure` | Decode failed; includes the last known trace state for diagnosis |
