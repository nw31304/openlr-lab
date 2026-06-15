# OpenLR Decode Engine — Design & Algorithms

This document describes the architecture, data flow, and key algorithmic decisions
in `crates/openlr-engine`. It is the canonical reference for resuming work on the
engine after a context gap.

---

## 1. Overview

The engine takes a `LocationReference` (the output of the codec layer — a list of
LRPs with interval-valued attributes) and a pre-loaded `Graph`, and returns a
`DecodedLocation` containing the matched path of segment IDs plus trimming offsets.

```
LocationReference  ──▶  select_candidates (per LRP)
                               │
                    all_candidates[0..N]
                               │
                    RouteGenerator (best-first combination iterator)
                               │
                    try_route_combination
                      ├── find_route (A* per leg)
                      └── validate_dnp (full LRP-to-LRP length)
                               │
                    apply_offset (pos / neg trim)
                               │
                    path_to_wkt (WKT assembly)
```

The two main public entry points are:
- `decode(loc_ref, graph, params)` → `Result<DecodedLocation, DecodeError>`
- `path_to_wkt(path, pos_offset_m, neg_offset_m, first_lrp_arc_m, last_lrp_arc_m, graph)` → `Option<String>`

---

## 2. Input: the unified LRP model

The codec layer (`openlr-codec`) normalises both v3 binary and TPEG-OLR formats into
a single `Lrp` struct where every quantised attribute is a `[LB, UB]` interval:

```rust
pub struct Lrp {
    pub coord:      (f64, f64),          // (lon, lat)
    pub bearing:    CircularInterval,    // degrees; mod-360 arithmetic
    pub frc:        u8,                  // 0 (motorway) … 7 (other)
    pub fow:        u8,
    pub lfrcnp:     Option<u8>,          // lowest FRC on path to next LRP (None on last)
    pub dnp:        Option<LinearInterval>, // distance to next LRP, meters (None on last)
    pub pos_offset: Option<LinearInterval>, // meters from first LRP to actual start
    pub neg_offset: Option<LinearInterval>, // meters before last LRP to actual end
}
```

**v3 encoding** fills `[LB, UB]` with the quantisation bucket:
- Bearing: 5-bit sector × 11.25° → 11.25° wide interval
- DNP: 1-byte bucket × 58.6 m → 58.6 m wide interval
- Offsets: same bucket scheme

**TPEG** sets `LB == UB` (point values — no quantisation).

All downstream code is format-agnostic; it operates on intervals, never raw bucket
indices. This is why `CircularInterval` and `LinearInterval` are distinct types
(Invariant 6 in CLAUDE.md).

---

## 3. Candidate selection (`candidate.rs`)

For each LRP the engine fetches nearby segments from the graph and scores each one
as a potential match. The result is a ranked list of `ScoredCandidate`.

### 3.1 Spatial fetch

```rust
graph.segments_near(lon, lat, candidate_search_radius_m)
```

This returns segments whose geometry bounding box comes within the search radius.
The exact projection distance is computed per-candidate; over-fetch is fine and
expected.

### 3.2 Traversal directions

A `Direction::Both` segment generates **two** candidates — one Forward, one
Backward. One-way segments generate only their legal direction. This doubles the
candidate set for bidirectional roads, which is necessary because the bearing check
is direction-dependent.

### 3.3 Projection and bearing

For each `(segment, direction)` pair:

1. **Reverse geometry** for Backward candidates. The geometry stored on the segment
   is always in the `start_node → end_node` direction. For a Backward candidate we
   reverse it before passing to `project_onto_polyline`.

2. **Project** the LRP coordinate onto the polyline to find the nearest point and
   arc-length offset.

3. **Compute bearing** at the projection using `bearing_at_offset(geom, arc_offset_m,
   forward)`:
   - Non-last LRPs: **forward** — 20 m ahead of projection point
   - Last LRP: **backward** — 20 m behind projection point (the "backward direction"
     convention in the OpenLR spec)

### 3.4 The `arc_offset_m` convention — CRITICAL

**`arc_offset_m` is always measured from the traversal entry, regardless of direction.**

Because Backward candidates reverse the geometry before projection:
- Forward: entry = stored start_node, arc measured from stored start
- Backward: entry = stored end_node, arc measured from stored END (not from start)

This means:
- `arc_offset_m` is the distance from traversal entry to the projection point
- `(seg_len - arc_offset_m)` is the distance from projection to traversal exit
- **No direction-conditional arithmetic is needed** — the formula is the same for
  both directions

These invariants are used in four places in `lib.rs`:

| Variable | Formula | Meaning |
|---|---|---|
| `from_partial` | `seg_len - arc_offset_m` | LRP projection → exit node |
| `to_partial` | `arc_offset_m` | entry node → LRP projection |
| `first_lrp_arc_m` | `arc_offset_m` | path start → first LRP position |
| `last_lrp_arc_m` | `arc_offset_m` | last segment entry → last LRP position |

**Bug history**: earlier versions had incorrect Backward-case inversions in all four
of these. The `match dir { Forward => arc, Backward => seg_len - arc }` pattern was
wrong because the geometry is already reversed before projection. The fix was to
remove the match entirely — all four formulas are direction-independent.

### 3.5 Hard gates and soft penalties

The candidate evaluation is a two-stage filter:

**Hard gates** (reject the candidate entirely if failed):
1. **Search radius**: `proj.distance_m > candidate_search_radius_m` → `FailRadius`
2. **Bearing**: `!widened_interval.contains(bearing)` → `FailBearing`
   - Widened interval = `[LB − τ, UB + τ]` where τ = `bearing_tolerance_deg`
   - For TPEG (LB == UB), the entire window comes from τ — without τ, every real
     candidate would be rejected (Invariant 5)

**Soft penalties** (admitted but ranked worse):
- `bearing_excess`: how far bearing lies outside `[LB, UB]` (zero if inside)
- `frc_penalty`: steps of FRC mismatch × `frc_penalty_per_step`
- `fow_penalty`: flat penalty if FOW doesn't match

**Total score** = `positional_distance_m + bearing_excess + frc_penalty + fow_penalty`

Lower score = better. Candidates are sorted ascending and truncated to
`max_candidates_per_lrp`. The truncation bounds the A\* search space from O(N^L)
to O(K^L) where K = max_candidates_per_lrp and L = LRP count.

---

## 4. Candidate combination search (`route_generator.rs`)

`RouteGenerator` is an iterator over `[usize; L]` index vectors. Each vector selects
one candidate per LRP. It yields combinations in ascending order of **total candidate
score sum** (best-first).

This is not a full priority queue over all O(K^L) combinations. It uses a
bounded-lookahead approach: it tracks the current combination and advances the
index at the leg with the smallest score improvement. This gives approximately
best-first ordering without materialising the full search space.

For most real references, the winning combination is found in the first 1–5 tries.
The worst case (all candidates have equal score) degenerates to lexicographic order.

---

## 5. Routing a single combination (`lib.rs::try_route_combination`)

For a given combination of candidates `[c0, c1, …, cL]`, this function routes
each leg `(c_i, c_{i+1})` and validates DNP.

### 5.1 Route cache

```rust
type RouteCache = HashMap<(NodeId, NodeId, u8), Option<(Vec<SegmentId>, f64)>>;
//                          exit    entry  lfrcnp   None=no path  Some=interior segs + length
```

Cache key = `(from.exit_node, to.entry_node, effective_lfrcnp)`.

**Only A\* failures (no path exists) are cached.** DNP failures are NOT cached
because the full path length depends on `from.arc_offset_m` and `to.arc_offset_m`,
which differ across candidate pairs sharing the same exit/entry nodes. If DNP
failures were cached, a valid candidate pair might be skipped because a previous
pair with the same nodes but different arc offsets produced a path that happened to
be too short/long.

### 5.2 Full path length computation

```
full_length_m = from_partial + interior_m + to_partial
```

Where:
- `from_partial = seg_len(from) - from.arc_offset_m`  (LRP position → exit node)
- `interior_m` = A\* route length (exit node → to's entry node)
- `to_partial = to.arc_offset_m`  (entry node → LRP position)

DNP validation runs against `full_length_m`, not `interior_m`. This was a bug in
early versions that caused valid routes to be rejected (the partial edges can be
hundreds of meters for mid-segment LRPs).

### 5.3 Path construction invariant

```
path = [from₀.segment_id,
        …interior₀_segments…,
        to₀.segment_id,    ← this equals from₁.segment_id
        …interior₁_segments…,
        to₁.segment_id,
        …]
```

The `to` segment of leg N equals the `from` segment of leg N+1. It appears exactly
once in the path (the interior segments returned by A\* do NOT include the to-segment;
the caller pushes it explicitly). This ensures the junction segment between legs is
present exactly once with no duplicates.

---

## 6. A\* routing (`astar.rs`)

### 6.1 State

State is `(NodeId, SegmentId)` — the node being expanded and the segment by which
it was reached. This is Invariant 3 of the project: it allows turn-restriction
checking at every expansion without retrofitting.

The closed set is keyed on `(node, via_seg)`. A node may be revisited via a
different incoming segment (different turn-restriction profile).

### 6.2 Seeding

The initial open-set element uses `from.segment_id` as the `via_seg`. This means
the very first turn-restriction check at `from.exit_node` fires correctly, as if
the traversal "arrived via" the from-candidate's segment.

### 6.3 Goal condition

```
node == to.entry_node  &&  via_seg != from.segment_id
```

The second clause prevents trivial self-matches when from and to share a node but
are different segments (e.g., a U-turn).

### 6.4 Successor generation (`graph.successors`)

For each outgoing segment from `node`, skip if:
- `seg.frc > lfrcnp` (LFRCNP floor — the key constraint ensuring ramps/connectors
  are available while lower-priority roads aren't mistakenly used)
- The turn `(via_seg → next_seg)` via `node` is prohibited by the restriction table
- `seg.direction` doesn't permit traversal from `node`

### 6.5 Distance bounds

- **A\* heuristic**: haversine distance from current node to goal node (admissible —
  never overestimates)
- **Hard upper bound**: `dnp.ub × max_path_search_factor` on cumulative g-cost
- **Expansion cap**: `max_astar_expansions` (prevents runaway on large graphs when
  the correct path is genuinely absent)

### 6.6 Path reconstruction

`reconstruct()` walks the closed list back to the root via parent pointers. It
strips:
- The `start_seg` (the from-candidate's partial edge, already in the path)
- The `to_seg` if A\* reached the goal by traversing it (prevents the duplicate
  that would otherwise appear when the caller also pushes `to.segment_id`)

---

## 7. DNP validation (`validation.rs::validate_dnp`)

```
half_bucket = (dnp.ub - dnp.lb) / 2.0           # half the encoding interval width
pct_tol     = path_length_m × dnp_tolerance_pct  # percentage-of-path tolerance
delta       = max(half_bucket, pct_tol)
window      = [dnp.lb - delta, dnp.ub + delta]
pass        = window.contains(path_length_m)
```

The two-term max is intentional:
- `half_bucket` dominates for short paths (v3 quantisation is coarse relative to the path)
- `pct_tol` dominates for long paths (map divergence accumulates with distance)

For TPEG (LB == UB, half_bucket == 0), only the percentage term is active.

---

## 8. Offset application and WKT assembly

### 8.1 apply_offset (`validation.rs`)

Uses the midpoint of the offset interval as the trim point:
```rust
trim_m = (offset_interval.lb + offset_interval.ub) / 2.0;
```

This is a placeholder — the correct approach is to treat the entire interval as a
range and report the resulting location uncertainty to the UI (especially important
for v3 where the 58.6 m bucket width means the real start could be ±29 m).

### 8.2 path_to_wkt (`wkt.rs`)

Takes the decoded path plus four offset values and produces a `LINESTRING` WKT.

**Inputs:**
- `path`: ordered `[SegmentId]`
- `pos_offset_m`: midpoint of positive offset interval (meters)
- `neg_offset_m`: midpoint of negative offset interval (meters)
- `first_lrp_arc_m`: `arc_offset_m` of the first candidate (traversal-direction offset)
- `last_lrp_arc_m`: `arc_offset_m` of the last candidate

**Trim computation:**

```
Forward start distance from path head = first_lrp_arc_m + pos_offset_m
Backward end distance from path tail  = last_lrp_arc_m - neg_offset_m
```

The `first_lrp_arc_m` offset is necessary because the path includes the full
first segment from its traversal-entry node, but the LRP (and hence the positive
offset origin) may be mid-segment. Without this, the positive-offset trim would be
measured from the segment node rather than the LRP projection.

Both trim values can overflow their segment — the excess carries into adjacent segments.

**Traversal direction inference:**

The stored path is a flat list of segment IDs. The function infers each segment's
traversal direction by walking node connectivity:
- Segment 0: look at which endpoint it shares with segment 1
- Segment i (i > 0): entry node = exit node of segment i−1

**Duplicate junction vertex deduplication:**

Adjacent segments share a junction node, whose coordinates appear in both geometries.
The second appearance is detected by coordinate equality (< 1e-8° tolerance) and
skipped.

---

## 9. Decode parameters (`params.rs`)

| Parameter | Type | Role |
|---|---|---|
| `candidate_search_radius_m` | f64 | Hard gate: max LRP-to-projection distance |
| `bearing_tolerance_deg` (τ) | f64 | Hard gate extension + map-divergence margin |
| `dnp_tolerance_pct` (δ) | f64 | DNP window percentage term |
| `frc_penalty_per_step` | f64 | Soft ranking weight |
| `fow_penalty` | f64 | Soft ranking weight (flat, not per-step) |
| `max_candidates_per_lrp` | usize | RouteGenerator search space bound |
| `max_path_search_factor` | f64 | A\* distance upper bound = dnp.ub × factor |
| `max_astar_expansions` | usize | Hard node-expansion cap (0 = unlimited) |
| `lfrcnp_tolerance` | u8 | Extra FRC steps added to encoded LFRCNP floor |
| `trace_level` | enum | Off / Summary / Full |

Three presets: **Permissive** (wide tolerances, cross-map decoding), **Default**,
**Strict** (tight, same-map decoding only).

The distinction between hard-gate parameters and soft-penalty parameters is
fundamental to the diagnostic capability: loosening a hard gate changes the
admissible candidate set (a discontinuous jump); changing a soft weight only
reorders the same set. The UI should make this distinction visible.

---

## 10. Trace system (`trace.rs`)

Every significant decision emits a `DecodeEvent` into the `DecodeTrace`. Events
fall into two verbosity levels:

- **Summary** (always emitted when trace is on): candidate counts, route found/failed,
  DNP check result, offset applied, decode complete
- **Full** (only when `trace_level == Full`): per-candidate evaluation, A\* node
  expansions, A\* edge skips

The trace is the foundation for the step-by-step debugger UI and for the diagnostic
verdict. It captures the exact margin by which each candidate passed or failed each
gate, making the closed-form "minimum required tolerance" computation possible.

Key event types:
- `CandidatesRanked { lrp_idx, accepted, rejected_count }` — accepted includes full
  scores; rejected are counted (detail available in Full mode)
- `RouteSearchStarted { leg, from, to }` — before A\*
- `RouteFound { leg, path, length_m }` — A\* + DNP both passed
- `RouteFailed { leg, reason }` — either A\* or DNP failed
- `DnpChecked { leg, interval, actual_m, passed }`
- `AStarNodeExpanded` / `AStarEdgeSkipped` — Full mode only
- `DecodeComplete(DecodeOutcome)` — terminal event with Success or failure reason

---

## 11. Key invariants summary

1. **arc_offset_m is from traversal entry, always.** No Backward-case inversion.
   `from_partial = seg_len - arc`, `to_partial = arc`, same formula for both directions.

2. **A\* state is (node, incoming_segment).** Required for turn restrictions from day one.

3. **Route cache does NOT cache DNP failures.** DNP depends on arc offsets that vary
   per candidate pair, even when the same (exit, entry, lfrcnp) triple is shared.

4. **full_length_m = from_partial + interior_m + to_partial.** Never validate DNP
   against interior_m alone — the partial edges can dominate for mid-segment LRPs.

5. **Junction segment appears exactly once in path.** A\* interior excludes to_seg;
   caller pushes it. No deduplication needed, no double-counting.

6. **The path includes from.segment_id.** The first call to `path.push(from.segment_id)`
   happens unconditionally before the A\* interior segments. This means the first
   and last segments in the path are always the from/to candidate segments respectively.

7. **WKT trim is relative to LRP arc position, not segment endpoint.** Use
   `first_lrp_arc_m + pos_offset_m` as the forward start, `last_lrp_arc_m - neg_offset_m`
   as the backward end. These values come directly from `candidate.projection.arc_offset_m`.

---

## 12. File map

| File | Contents |
|---|---|
| `lib.rs` | `decode()` entry point, `try_route_combination()`, `DecodedLocation`, `DecodeError` |
| `candidate.rs` | `select_candidates()`, `evaluate_candidate()`, bearing/projection scoring |
| `astar.rs` | `find_route()`, path reconstruction, A\* state machine |
| `route_generator.rs` | `RouteGenerator` — best-first combination iterator |
| `validation.rs` | `validate_dnp()`, `apply_offset()`, `path_length_m()` |
| `wkt.rs` | `path_to_wkt()`, `segment_vertices()` |
| `params.rs` | `DecodeParams`, `Preset` |
| `trace.rs` | `DecodeTrace`, `DecodeEvent`, `ScoredCandidate`, `TraversalDir`, etc. |
| `tile_prefetch.rs` | `prefetch_tile_keys()` — compute tiles needed before decode |
| `diagnostics.rs` | Stub for the desired-vs-actual root-cause analysis |

---

## 13. Known limitations / next steps

- **`apply_offset` uses interval midpoint** — should use the full interval to
  report the location uncertainty range to the UI (especially material for v3's
  ±29 m bucket half-width).

- **`diagnostics.rs` is a stub** — the desired-vs-actual forced-decode, feasibility
  margin computation, and LP-based ranking flip check are not yet implemented.

- **RouteGenerator ordering** is approximately best-first, not exactly. For
  references with many LRPs and many candidates, the first winning combination may
  not be the globally optimal one. This is acceptable for v1.

- **WASM steppability** — the decode loop is not yet steppable (pause/resume for UI
  animation). The architecture is already structured for this (trace events are
  emitted in order) but the `decode()` function is synchronous and blocking. The
  WASM wrapper will need to split it into steps.
