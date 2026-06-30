# OpenLR Decode Diagnostics — LLM Context Document

This document gives you everything you need to diagnose an OpenLR decode result.
Read it before calling any tools. It describes the decode algorithm, the scoring
model, the failure taxonomy, and the format of all data you will receive.

---

## Available tools

Call these in the order that minimises token usage. Always start with
`get_decode_summary`. Drill down only into the areas relevant to the failure.

| Tool | When to call |
|---|---|
| `get_decode_summary()` | **Always first.** Status, format, LRP/leg counts, outcome. |
| `get_parsed_reference()` | When you need to see exactly what was encoded (LRP coords, FRC, FOW, bearing/DNP intervals). |
| `get_lrp_candidates(lrp_index, include_rejected?)` | When a candidate selection failure is suspected. Start with `include_rejected=false`; add rejected candidates only if accepted set doesn't explain the problem. |
| `get_leg_summary(leg_index)` | When routing failed or DNP validation failed on a leg. |
| `get_route_segments(leg_index)` | When you need the per-segment breakdown of a successfully found route (FRC, FOW, cumulative length). |
| `get_astar_trace(leg_index, offset, limit)` | Paginated A\* expansion/skip events. Request first page (limit=50) to see where search stalled; fetch more only if needed. |
| `get_segment(segment_id)` | Full attributes + geometry for one segment. |
| `get_segments_near(lat, lon, radius_m)` | Map query: segments near a coordinate. |
| `rescore_candidate(lrp_index, segment_id, params_override)` | Test whether a rejected segment would pass with modified parameters. |
| `retry_decode(params_override)` | Re-run full decode with modified parameters against already-loaded tiles. Returns a new summary; drill down with other tools. |
| `retry_leg(leg_index, from_segment_id, to_segment_id, params_override)` | Force a specific candidate pair for one leg and re-run A\*. The forced-decode tool: use it to test whether the user's intended path is feasible. |

All tools return data in **TOON format** (Token-Oriented Object Notation).
TOON is a compact, lossless JSON representation. Uniform arrays are expressed
as a header row of field names followed by one data row per item — far more
token-efficient than repeated JSON objects.

---

## Scoring model

**Lower score = better match. Zero = perfect.**

All penalties add to the score; nothing subtracts. The total score is additive
and decomposable:

```
total = distance_score + bearing_score + frc_score + fow_score
      + interior_score + wrong_endpoint_score
```

Each term is independently attributable. When diagnosing why one candidate
outscored another, attribute the gap to specific terms.

### Hard gates vs. soft penalties

**Hard gate** — a candidate that fails is rejected outright. It does not appear
in the accepted list and cannot be selected regardless of how other terms score.
Gates are: search radius, segment direction (one-way), bearing window, total
score ceiling.

**Soft penalty** — a candidate that passes all gates is ranked by its total
score. High-scoring (worse) candidates lose to lower-scoring (better) ones,
but all are eligible.

When diagnosing: distinguish between "this segment was never considered" (hard
gate failure) and "this segment was considered but outscored" (soft penalty).
The remedies are different.

---

## Format differences: TomTom v3 vs. TPEG-OLR

| Property | TomTom binary v3 | TPEG-OLR (ISO 21219-22) |
|---|---|---|
| Bearing precision | 5-bit, 32 sectors × 11.25° — each value is a bucket `[LB, UB]` of width 11.25° | Full precision — `LB == UB` (a point, not a bucket) |
| DNP precision | 1-byte, ~58.6 m buckets — each value is a `[LB, UB]` interval | Exact — `LB == UB` |
| Offset precision | Same ~58.6 m buckets | Exact |

A candidate sitting at the edge of a v3 bearing interval is **not necessarily
a poor match** — it may simply be at the edge of a quantisation bucket. Always
check whether a marginal bearing or DNP score is within one bucket width of the
interval before concluding it reflects genuine map divergence.

---

## Step 1 — Parse

The encoded string is parsed into a `LocationReference` of N Location Reference
Points (LRPs). Each LRP carries:
- Coordinate (lon, lat)
- FRC (Functional Road Class, 0–7; 0 = motorway, 7 = local path)
- FOW (Form of Way: 0=undefined, 1=motorway, 2=dual carriageway,
  3=single carriageway, 4=roundabout, 5=traffic square, 6=slip road, 7=other)
- Bearing `[LB, UB]` degrees clockwise from north
- DNP `[LB, UB]` metres (all LRPs except the last)
- LFRCNP: lowest FRC the A\* path to the next LRP may use (all except last)

### Parse failures

Tags: 📝 = encoding deficiency

- Invalid or truncated base64/hex string 📝
- Unsupported format variant or version 📝
- Corrupt binary (bit-flip, truncation) 📝

No trace events are emitted on parse failure. The raw error string is the only
diagnostic.

---

## Step 2 — Candidate selection (per LRP)

### 2a. Spatial fetch

All segments within `candidate_search_radius_m` of the LRP coordinate are fetched.
Zero results → abort.

**Failure: no segments fetched**
Tags: 🔧 = decoder-tunable, 🗺️ = map deficiency

- Tile not built or not loaded for this region 🗺️
- Tile fetch failed silently (network error, wrong archive URL) 🔧 *(check tile load events)*
- Tile boundary gap — LRP sits near a tile edge and the nearest segment is in
  an adjacent tile that wasn't loaded 🔧
- No road segments in this area (unmapped, remote) 🗺️
- Search radius too small — segments exist beyond the configured radius 🔧
  *(suggest increasing `candidate_search_radius_m`)*

*Distinguish from §2e failures by `segments_fetched` in `CandidatesRanked`:
zero = no roads fetched; non-zero = roads fetched but all rejected.*

### 2b. Snap point determination

For each fetched segment:
1. Project the LRP coordinate onto the segment polyline → nearest interior point.
2. If the projected point is within `snap_to_endpoint_threshold_m` of either
   endpoint, **shift the snap to that endpoint**.
3. Otherwise the snap remains in the interior of the segment.

An interior snap means A\* departs from (or arrives at) a mid-segment position.
An endpoint snap anchors routing to a known graph node.

### 2c. Bearing computation

The bearing used for scoring is computed **at the snap point**, not at the segment
endpoint. Starting from the snap point, measure the heading along segment geometry
over the **next 20 m** in the direction of travel (forward for non-terminal LRPs;
backward for the terminal LRP).

> A segment that looks wrong when measured from its start node may look correct
> when measured from a mid-segment snap point. If a bearing score is surprising,
> check `snap_lon`/`snap_lat` and `bearing_deg` in the candidate data.

### 2d. Candidate scoring (soft penalties)

| Field | What it measures |
|---|---|
| `distance_score` | `distance_weight × (distance_m / search_radius_m)` — primary ranking term |
| `bearing_score` | `bearing_weight × bucket_delta × bearing_penalty_per_bucket` |
| `frc_score` | `frc_weight × frc_penalty_table[lrp_frc][seg_frc]` |
| `fow_score` | `fow_weight × fow_penalty_table[lrp_fow][seg_fow]` |
| `interior_score` | `interior_weight` when snapped to interior; 0 at endpoints |
| `wrong_endpoint_score` | `wrong_endpoint_weight × position_along_segment`; 0 at the correct end |

**Wrong endpoint** — for a non-terminal LRP the expected snap is near the **start**
of the segment in the direction of travel. For the terminal LRP it is near the
**end** in the direction of travel. Snapping to the other end is penalised.

**DNP is not scored per candidate.** It is validated after routing (Step 3).

### 2e. Hard gates

A candidate is **rejected** (not penalised — removed entirely) if:
- Its projection distance exceeds the search radius (`FailRadius`)
- The segment geometry is degenerate (`FailDirection`)
- The computed bearing falls outside `[LB − τ, UB + τ]` where τ = `max_bearing_deviation_deg` (`FailBearing`)
- The total score exceeds `max_candidate_score` (`FailScore`)

**Failure: candidates fetched but all rejected**
Tags: 🔧 🗺️

- Bearing tolerance too tight — all candidates fail `FailBearing` 🔧
  *(check `excess_deg` in rejected candidates; suggest increasing `max_bearing_deviation_deg`)*
- One-way roads digitised in the wrong direction — bearing ~180° off 🗺️
- LRP on a tightly curved road — projected bearing differs from encoded bearing 📝🗺️
- Score threshold too low — candidates pass bearing but fail `FailScore` 🔧
- FRC/FOW misattribution in target map 🗺️

---

## Step 3 — Route search

### 3a. Candidate permutation order

Combinations of candidates (one per LRP) are tried in **ascending combined
score** order — lowest (best) total first. The most promising combination is
tried first.

### 3b. A\* routing

For each adjacent LRP pair in the selected combination, A\* finds the shortest
path:

- **State:** `(graph_node, incoming_segment)` — required to honour turn
  restrictions. A prohibited `(from_seg, via_node, to_seg)` triple can only be
  detected if the incoming segment is tracked.
- **LFRCNP hard gate:** at each expansion, any edge whose FRC is worse (higher
  number) than the leg's effective LFRCNP (`lfrcnp + lfrcnp_tolerance`) is
  **skipped entirely**. This is a per-edge hard gate, not a global filter.
- **DNP termination:** if cumulative route length exceeds `DNP_UB × max_path_search_factor`,
  A\* terminates for this candidate pair.
- **Expansion cap:** if `max_astar_expansions` is reached, A\* terminates.
- **Results are cached** per `(exit_node, entry_node, lfrcnp)` — the same node
  pair is never routed twice across different permutations.

### 3c. Interpreting AStarTerminated

The `AStarTerminated` event (Summary level) carries four skip counters. Use them
to diagnose without needing the full expansion trace:

| Signal | Likely cause |
|---|---|
| `edges_skipped_frc` dominates, `nodes_expanded` very small (1–3) | LFRCNP ceiling blocking all exits — raise `lfrcnp_tolerance` 🔧 |
| `edges_skipped_turn` > 0, `nodes_expanded` small | Turn restriction blocking the route — restriction correct but path needs intermediate LRP 📝, or restriction wrong in map 🗺️ |
| `edges_skipped_distance` high, `reason=OpenSetExhausted` | Route exists but detours significantly — raise `max_path_search_factor` 🔧 |
| `reason=ExpansionLimitHit` | Search cut short — raise `max_astar_expansions` 🔧 |
| All skip counters near zero, `nodes_expanded` very small (1–3) | Graph island — segment has no outgoing connections (missing tile, boundary stitch failure, wrong one-way direction) 🔧🗺️ |

### 3d. DNP validation

After a route is found, its length must fall within `[DNP_LB − δ, DNP_UB + δ]`
where δ = `max(dnp_bucket_half, dnp_tolerance_pct × route_length)`.

**Failure: route found but DNP out of range**
Tags: 🔧 🗺️ 📝

- DNP tolerance too tight 🔧 *(suggest increasing `dnp_tolerance_pct`)*
- v3 bucket quantisation — legitimate path sits near a bucket edge 📝
- Route detour — A\* found a plausible-but-longer path via a missing shortcut 🗺️
- Road geometry differs between source and target map (realignment, construction) 🗺️

### 3e. Routing failures summary

If all candidate combinations fail (or `max_routing_attempts` cap fires):

- Increase `lfrcnp_tolerance` — allows A\* to use lower-class connectors 🔧
- Increase `max_path_search_factor` — allows longer detour paths 🔧
- Increase `max_routing_attempts` — allows more candidate combinations 🔧
- Map may be missing a connecting segment 🗺️

---

## Step 4 — Offset trimming

The found path runs from the first LRP snap to the last. Offsets trim it:

- **Positive offset** trims from the **path start** (first LRP end).
- **Negative offset** trims from the **path end** (last LRP end).
- Each offset is a `[LB, UB]` interval (v3 bucket; exact for TPEG).
- The decoder uses **LB** of each offset — minimum trim, maximum reported location.
- The **uncertainty** in the final location boundary = `UB − LB` at each end.

**Failure: offset over-trims**

- `LB(pos) + LB(neg) > route_length` — offsets are larger than the path 📝🗺️
- v3 offset bucket spans a segment boundary — trim point is ambiguous 📝

---

## Silent misdecode — success returned, wrong path highlighted

No error is returned. This is the most important failure class. The decoder
reports success but highlights the wrong road.

### Diagnosis workflow

1. Use `retry_leg` with the intended segment pair to test whether the correct
   path is **feasible** (passes all hard gates and DNP).
2. If feasible: the correct path loses on score — identify the gap term-by-term
   using `get_lrp_candidates` and `rescore_candidate`. Test whether parameter
   changes close the gap with `retry_decode`.
3. If infeasible: identify which gate fails and by how much. If no parameter
   change can make it feasible → encoding deficiency (an intermediate LRP is
   needed).

### Root-cause tags

| Verdict | Meaning |
|---|---|
| 🔧 Decoder-tunable | A parameter combination makes the correct path the strict unique winner. The required parameter change is the recommendation. |
| 🗺️ Map deficiency | The correct path is infeasible due to a missing, misclassified, or misdirected segment in the target map. |
| 📝 Encoding deficiency | No parameter change recovers the correct path. An additional or repositioned LRP is needed in the reference. |

### Wrong candidate at an LRP

*Visible in `get_lrp_candidates` score table.*

- Correct candidate passes all gates but is outranked by a wrong one
  - Score gap closable by reweighting → 🔧
  - Score gap not closable → intermediate LRP needed at this junction 📝
- Correct candidate near a v3 bearing sector boundary — measured in adjacent sector 📝

### Correct candidate, wrong route

*Visible in `get_route_segments` and `get_astar_trace`.*

- Wrong candidate combination routes successfully and passes DNP; correct
  combination fails 🗺️📝
- Intended road missing or one-way wrong in target map; A\* detours 🗺️
- Intermediate LRP too far from the actual junction — branches indistinguishable
  from the LRP position 📝

### Correct path, wrong offset

- Large offset moves the start/end point past an intended junction 📝

---

## Trace event reference

Events are emitted into a flat `events` array in chronological order.
The TOON examples below show how tools return grouped, tabular subsets of
this data — not the raw flat array.

### `CandidateSearchStarted` (Summary)
```
lrp_idx: 0
coord_lon: 7.73076
coord_lat: 48.05086
radius_m: 50.0
```

### `CandidatesRanked` (Summary)
Returned by `get_lrp_candidates`. Accepted candidates include full score
decomposition and snap geometry.

```
lrp_idx: 0
segments_fetched: 12

accepted[2]{seg_id,traversal,snap_lon,snap_lat,arc_m,dist_m,bearing_deg,at_entry,at_exit,d_sc,b_sc,frc_sc,fow_sc,int_sc,wep_sc,total}:
  12441,Forward,7.73082,48.05091,145.3,3.2,162.3,false,false,0.80,0.00,0.00,0.50,0.20,0.00,1.50
  12219,Forward,7.73071,48.05079,203.1,8.7,159.1,false,false,2.10,0.00,0.00,0.50,0.20,0.00,2.80

rejected[3]{seg_id,traversal,dist_m,bearing_deg,verdict,excess}:
  11803,Forward,12.1,201.4,FailBearing,33.2
  11804,Backward,null,null,FailDirection,null
  11901,Forward,18.3,163.1,FailScore,4.2
```

*`excess` = degrees over bearing gate, score over score gate, or metres over radius gate.*

### `RouteSearchStarted` (Summary)
```
leg: 0
from_seg_id: 12441
to_seg_id: 14081
```

### `AStarTerminated` (Summary)
```
leg: 0
reason: OpenSetExhausted
nodes_expanded: 1247
edges_skipped_frc: 83
edges_skipped_direction: 0
edges_skipped_turn: 2
edges_skipped_distance: 41
```

### `RouteFound` (Summary)
Returned by `get_route_segments` with per-segment detail.

```
leg: 0
length_m: 5831.1
attempt_index: 2
from_snap_lon: 7.73082
from_snap_lat: 48.05091
to_snap_lon: 7.76814
to_snap_lat: 48.04795

segments[6]{seg_id,frc,fow,length_m,cumulative_m}:
  12441,3,3,145.3,145.3
  12553,3,3,892.1,1037.4
  12891,2,2,2204.7,3242.1
  13012,3,3,412.8,3654.9
  13108,3,3,1801.3,5456.2
  13219,3,3,374.9,5831.1
```

### `RouteFailed` (Summary)
```
leg: 0
reason: NoPathFound | DnpOutOfRange | NeedsTile
# DnpOutOfRange carries:
actual_m: 6201.4
window_lb: 5772.0
window_ub: 5888.0
```

### `DnpChecked` (Summary)
```
leg: 0
interval_lb: 5801.0
interval_ub: 5859.0
actual_m: 5831.2
passed: true
```

### `OffsetApplied` (Summary)
```
is_positive: true
interval_lb: 0.0
interval_ub: 58.6
trim_lon: 7.73201
trim_lat: 48.05103
```

### `RouteAttemptsExhausted` (Summary)
```
limit: 10
attempted: 10
```

### `TileLoaded` (Summary)
```
z: 12
x: 2173
y: 1466
segment_count: 3842
```

### `TileNeeded` / `TileFailed` (Summary)
```
z: 12
x: 2174
y: 1466
# TileFailed also carries:
reason: NetworkError | NotFound
```

### `AStarNodeExpanded` (Full only — via `get_astar_trace`)
```
expansions[N]{node,via_seg,g_m,h_m,lon,lat}:
  1041,12441,0.0,5831.1,7.73082,48.05091
  1042,12553,145.3,5685.8,7.73201,48.05103
  ...
```

### `AStarEdgeSkipped` (Full only — via `get_astar_trace`)
```
skips[N]{from_node,seg,reason,detail}:
  1044,11901,FrcBelowLfrcnp,"seg=5 lfrcnp=3"
  1044,11902,TurnRestricted,null
  1051,12109,ExceedsMaxDistance,"6241.3>5888.0"
```

### `DecodeComplete` (Summary)
```
outcome: Success | NoCandidates | NoRoute
# NoCandidates:
failed_lrp_idx: 1
# NoRoute:
failed_leg: 0
# Success:
pos_offset_lb: 0.0
pos_offset_ub: 58.6
neg_offset_lb: 0.0
neg_offset_ub: 0.0
```

---

## Decode parameters reference

All parameters are tunable at decode time. Key parameters and their effect:

| Parameter | Default | What it controls |
|---|---|---|
| `candidate_search_radius_m` | 50 | Spatial fetch radius around each LRP |
| `snap_to_endpoint_threshold_m` | 10 | Distance at which interior snap shifts to endpoint |
| `max_bearing_deviation_deg` (τ) | 25 | Hard bearing gate half-width beyond `[LB, UB]` |
| `max_candidate_score` | 15 | Hard gate on total candidate score |
| `max_candidates_per_lrp` | 5 | How many candidates to retain per LRP |
| `dnp_tolerance_pct` (δ) | 0.1 | DNP window expansion as fraction of route length |
| `lfrcnp_tolerance` | 0 | Relaxes LFRCNP: A\* may use roads up to `lfrcnp + tolerance` levels worse |
| `max_path_search_factor` | 1.5 | A\* caps at `DNP_UB × factor` |
| `max_astar_expansions` | 5000 | Per-leg expansion cap |
| `max_routing_attempts` | 10 | Global cap on candidate combinations tried (0 = unlimited) |

---

## Typical diagnostic session

```
get_decode_summary()                     →  ~150 tokens
get_parsed_reference()                   →  ~200 tokens   (if LRP data needed)
get_lrp_candidates(1)                    →  ~300 tokens   (bearing rejection visible)
rescore_candidate(1, 11803, {τ: 40})     →  ~80 tokens    (confirms gate passes)
retry_decode({max_bearing_deviation: 30}) →  ~150 tokens  (confirms decode succeeds)
                                         ──────────────
Total                                       ~880 tokens
```

A Full-level trace dump for a complex urban decode: 20,000–100,000 tokens.
The incremental pull model makes LLM diagnosis economically viable.
