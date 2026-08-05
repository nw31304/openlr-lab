# OpenLR Decode Failure Taxonomy

Tags: 🔧 decoder-tunable · 🗺️ map deficiency · 📝 encoding deficiency  
Annotation: **[auto]** = currently surfaced by the UI failure diagnosis · **[trace]** = visible in the trace panel · **[future]** = requires the planned automated root-cause verdict (closed-form/LP tuning check, not yet implemented — forced-decode mode itself is already shipped, see §8)

A decode can fail hard (an error is returned) or fail silently (success returned
but the wrong path is highlighted). Silent misdecoders are the most important
class to diagnose — they are the primary motivation for this tool.

---

## 1. Parse failure

*Trace events: none — failure occurs before decode begins.*

- Invalid or truncated base64 / hex string 📝
- Unsupported format variant or version 📝
- Corrupt binary (bit-flip, truncation) 📝

The raw error string is the only diagnostic available here; the failure popup
shows it directly. No trace-based enrichment is possible.

---

## 2. No candidate segments found

*Trace events: `CandidateSearchStarted`, `CandidatesRanked` (`segments_fetched = 0`).*

The spatial query returns zero segments near an LRP, so no evaluation is
attempted. Distinguish from §3 (segments fetched but all rejected) by the
`segments_fetched` counter in the `CandidatesRanked` event. **[auto]**

- **Tile not built or not loaded**
  - Tile store has no coverage for the region 🗺️
  - Tile fetch failed silently (network error, CORS, wrong archive URL) 🔧
    *Tile fetch errors are logged to the browser console but not yet surfaced
    in the UI — a tile-load summary trace event would close this gap.*
  - Tile boundary stitching gap — LRP sits near a tile edge and the nearest
    segment lives in the unloaded adjacent tile 🔧
- **No road segments in the loaded tile**
  - Genuinely unmapped or unpaved area 🗺️
  - Low-FRC roads (tracks, footways) intentionally omitted from the tile store 🗺️
- **Search radius too small** — segments exist but their nearest projection
  falls beyond the candidate radius 🔧 **[auto: suggests increasing radius]**

---

## 3. Candidates found but all rejected by hard gates

*Trace events: `CandidatesRanked` (`segments_fetched > 0`, `accepted` empty,
`rejected` list populated with per-candidate verdicts at Full trace level).*

Segments are returned by the spatial query but none survive to the accepted
list. The `rejected` array in `CandidatesRanked` carries a `verdict` for each
candidate; the failure popup groups these by reason. **[auto]**

### Bearing gate failures (`FailBearing`)

*`verdict.FailBearing.excess_deg` — degrees over the `max_bearing_deviation_deg` limit.*

- LRP bearing encoded near a v3 sector boundary (11.25°) — the decoder
  measures a bearing in the adjacent sector 📝
- One-way road digitized in the wrong direction — candidate bearing is ~180°
  off the encoded value 🗺️
- Bearing tolerance (`max_bearing_deviation_deg`) set too tight 🔧 **[auto: suggests increasing tolerance]**
- LRP placed on a tightly curved road or roundabout — projected bearing
  differs from the approach bearing the encoder intended 📝🗺️
- Extreme source/target geometry divergence (e.g., different overpass or
  interchange shape) 🗺️

### Score gate failures (`FailScore`)

*`verdict.FailScore.total` — combined penalty that exceeded `max_candidate_score`.*

Combined FRC + FOW + bearing + distance penalties exceed `max_candidate_score`.

- `max_candidate_score` threshold set too low 🔧 **[auto: suggests raising threshold]**
- FRC or FOW weight or penalty table too aggressive 🔧
- FRC misattribution: segment FRC in target map differs from LRP-encoded FRC 🗺️
- FOW misattribution: segment FOW in target map differs from LRP-encoded FOW 🗺️
- Dual carriageway not represented as FOW=2 in target map 🗺️
- Roundabout not represented as FOW=4 in target map 🗺️

---

## 4. No route found between an LRP pair

*Trace events: `RouteSearchStarted`, `AStarTerminated` (at Summary trace level —
no Full trace required), `RouteFailed`.*

Candidates exist at both LRPs but A\* cannot find a connecting path. The
`AStarTerminated` event is the primary diagnostic; it carries `reason`,
`nodes_expanded`, and five skip counters even at Summary trace level. **[auto]**

```
AStarTerminated {
  reason:                    OpenSetExhausted | ExpansionLimitHit { limit }
  nodes_expanded:            u32
  edges_skipped_frc:         u32   // FRC > LFRCNP ceiling
  edges_skipped_turn:        u32   // explicit turn restriction
  edges_skipped_direction:   u32   // one-way direction violation
  edges_skipped_distance:    u32   // path length > dnp.ub × max_path_search_factor
  edges_skipped_sharp_turn:  u32   // turn angle > max_interior_turn_deviation_deg
}
```

### FRC / LFRCNP blocking all exits

*Signal: `edges_skipped_frc` dominates, `nodes_expanded` is very small (often 1).*

- Effective LFRCNP ceiling too low: `lfrcnp + lfrcnp_tolerance < FRC of roads
  on the actual path` — raise `lfrcnp_tolerance` 🔧 **[auto: suggests lowering LFRCNP floor]**
- FRC misattribution: connecting roads are classified at a worse FRC than they
  actually are in the target map 🗺️

### Turn restriction blocks all exits

*Signal: `edges_skipped_turn` > 0, `nodes_expanded` is small.*

- Restriction is correct but the encoded path is infeasible — encoding
  deficiency (the reference needs an intermediate LRP to route around the
  restriction) 📝
- Restriction incorrectly modelled in the target map 🗺️ **[auto: reported in bullets]**

### Turn-angle gate blocks all exits

*Signal: `edges_skipped_sharp_turn` > 0, `nodes_expanded` is small.*

- The only physical continuation requires a turn sharper than
  `max_interior_turn_deviation_deg` (e.g. a dead end forcing a near-U-turn) —
  raise the cap 🔧 **[auto: suggests raising max_interior_turn_deviation_deg]**
- The encoded reference genuinely requires a turn no real vehicle could make
  there — encoding deficiency 📝

### A\* search budget exhausted

- **Expansion limit hit** (`reason = ExpansionLimitHit`) — the search was
  cut short; the path may exist but was not reached 🔧 **[auto: suggests raising max_astar_expansions]**
- **Distance cap exceeded** (`edges_skipped_distance` high, `reason =
  OpenSetExhausted`) — the route detours significantly; raise
  `max_path_search_factor` 🔧 **[auto: suggests raising factor]**
- **Route genuinely impossible within the encoded DNP window** — the encoder
  underestimated the distance or omitted a required intermediate LRP 📝

### Graph disconnection

*Signal: `nodes_expanded` is very small (1–3) despite `edges_skipped_*` all near
zero — A\* ran out of successors immediately.*

- One-way road digitized in the wrong direction — A\* cannot traverse the
  required segment even though it exists 🗺️
- Road missing from target map — required link is simply absent 🗺️
- Candidate segment is a graph island — no outgoing connections in the loaded
  tiles (missing boundary stitching or missing adjacent tile load) 🔧🗺️

Graph disconnection is currently folded into the generic exhaustion message.
A dedicated heuristic — comparing `nodes_expanded` against the total skip
counts — could surface this as its own diagnosis bucket.

---

## 5. Decode aborted — dynamic tile-load cap exceeded

*Trace events: none — the abort happens client-side, before any `decode()` call
completes with a final result. `AStarTerminated`/`RouteFailed` are never reached,
so there is no trace to inspect for this attempt.*

A\* aborts its **entire** search and the browser restarts `decode()` from scratch
the moment A\* pops any boundary node needing a tile that isn't loaded yet — even
a node on a dead-end branch that will never end up on the real path. If several
dead-end branches each need a different unloaded tile, the browser can burn
through many restart cycles without ever completing a single uninterrupted A\*
run, and hits its dynamic-tile-load cap (`MAX_DYNAMIC_LOADS`, currently 20)
before A\* ever reaches its own termination. **[auto]**

- **Heuristic can't see obstacles** — the A\* heuristic is straight-line
  distance to the goal; it has no notion of impassable terrain (a body of
  water, a border, a mountain range). Candidates near the "wrong" side of such
  an obstacle look promising (low heuristic) but are dead ends requiring a long
  detour, and each one visited can trigger its own restart 🔧🗺️
- **`max_path_search_factor` too high** — each individual run is still bounded
  to `dnp.ub × max_path_search_factor` (§4's distance cap), but a larger factor
  widens the blast radius of every dead-end branch, increasing the chance any
  one of them hits an unloaded tile before the real path is found. **Lowering
  the factor is the fastest first step** — it makes a run far more likely to
  complete without hitting this cap at all, which is what actually surfaces
  trace data to diagnose the real problem 🔧 **[auto: suggested first in the
  failure message]**
- **The real underlying issue is often something else entirely** — hitting
  this cap only prevents *seeing* the actual failure mode, it isn't the failure
  mode itself. A permissive `lfrcnp_tolerance` letting A\* wander through many
  low-class connector roads is a common root cause, only visible once a smaller
  search factor lets a run complete far enough to reach §4's diagnostics 🔧

---

## 6. Route found but DNP validation fails

*Trace events: `DnpChecked` (actual vs. window), `RouteFailed` with
`reason.DnpOutOfRange { actual_m, window }`.* **[auto]**

A path was found but its length falls outside the allowed window `[LB±δ, UB±δ]`.
The overshoot or undershoot in metres is shown directly.

- **DNP tolerance too tight** (`dnp_tolerance_pct` too small) 🔧 **[auto: suggests increasing tolerance]**
- **v3 bucket quantisation error** — the encoded DNP bucket is ~58.6 m wide;
  the actual path length may legitimately sit near a bucket edge 📝
- **Wrong route chosen by A\*** — a plausible path routes successfully and
  passes DNP gates but is not the intended one 🗺️📝
- **Road geometry differs between source and target map**
  - Lossy simplification in one map introduces cumulative length error 🗺️
  - Road realignment, rerouting, or construction since encoding 🗺️
- **Partial edge contribution error** — arc offset on the from/to candidate
  segment is incorrect, skewing the full LRP-to-LRP length calculation 🔧

---

## 7. Offset trimming failure

*Trace events: `OffsetApplied` (trim amount and interval).* **[trace]**

The decode finds the correct path but offset application produces an invalid or
unexpected result.

- **Offset larger than the first or last segment** — the trim point falls beyond
  the segment end; the decoder must walk forward into the next segment 📝🗺️
- **v3 offset bucket spans a segment boundary** — the `[LB, UB]` interval
  straddles the end of the first segment; the trimmed location is ambiguous 📝

A dedicated `OffsetFailed` trace event (not yet emitted) would make these
distinguishable from a successful offset that happens to land at a surprising
location.

---

## 8. Silent misdecode — success returned, wrong path highlighted

*No error is returned. This is the most important class to diagnose and the
primary reason this tool exists.*

**Forced-decode mode is implemented**: pin any candidate per LRP (📌 in the
TracePanel candidate table) and re-run A* against just those pinned
candidates (`▶ Re-run with pinned candidates`), or drive the equivalent
`set_pinned_candidates` + `run_forced_decode` tools from the LLM chat. This
lets you test directly whether the path you expected is even feasible, and
if so, why the decoder didn't pick it — but the comparison against the
winning path is still manual (read the score tables side by side), not an
automated verdict.

The **automated root-cause verdict** described below — mechanically proving
*decoder-tunable* vs. *encoder-deficient* — is **not implemented**. It would
require:

- **Decoder-tunable**: some parameter combination makes the correct path the
  strict unique winner — identified via closed-form gate margins and a linear
  feasibility check over the weight box.
- **Encoding-deficient**: no parameter combination recovers the correct path —
  the reference needs an additional or repositioned LRP, reported with a
  proof that no tuning recovers it.

The AI Chat button in the ResultPanel (see `WebFrontend.md`) is the closest
thing to this today: `buildSystemContext`/`buildEncodeDiagnosticPrompt` inject
the full trace (and, in encode mode, the waypoint/route/verify state) into
the LLM context so the model can reason about candidate scores, route
choices, and parameter sensitivities, including calling
`set_pinned_candidates`/`run_forced_decode` itself.
This is a human-in-the-loop tool, not the closed-form proof above.

### Wrong candidate selected at an LRP

*Visible in the trace panel: score table in the Candidates section.*

- Correct candidate passes all gates but is outranked by a plausible wrong one
  - Score gap closable by reweighting FRC/FOW/bearing/distance terms 🔧 **[future]**
  - Score gap not closable by any allowed weight vector → intermediate LRP
    needed at this junction 📝 **[future]**
- Correct candidate is near a v3 bearing sector edge; decoder measures it in
  the adjacent sector and scores it as a bearing deviation 📝

### Correct candidate selected but wrong route taken

*Visible in the trace panel: routing section, path segment list.*

- Wrong from/to candidate combination routes successfully and passes DNP while
  the correct combination fails 🗺️📝
- Intended road missing or one-way wrong in target map; A\* detours via a
  different path that happens to satisfy DNP 🗺️
- Intermediate LRP placed too far from the actual junction — the correct branch
  and an incorrect branch are geometrically indistinguishable from the LRP
  position 📝

### Correct path found but offset trims to wrong location

- Path assembly is correct but a large offset moves the start/end point past an
  intended junction 📝

---

## Implementation notes

| Failure class | Trace level needed | Auto-diagnosed | Planned |
|---|---|---|---|
| §1 Parse failure | — | raw error string | — |
| §2 Coverage gap | Summary | ✅ | tile-load summary event |
| §2 Radius too small | Summary | ✅ | — |
| §3 Bearing/score rejection | Summary (counts) / Full (per-candidate) | ✅ | — |
| §4 FRC blocking | Summary | ✅ | — |
| §4 Turn restriction | Summary | ✅ | — |
| §4 Expansion limit | Summary | ✅ | — |
| §4 Distance cap | Summary | ✅ | — |
| §4 Graph disconnection | Summary (inferred) | ⚠️ heuristic gap | node/skip ratio heuristic |
| §5 Tile-load cap exceeded | none (aborts before any trace) | ✅ suggests reducing search factor | skip-and-continue instead of full restart |
| §6 DNP mismatch | Summary | ✅ | — |
| §7 Offset trimming | Summary | trace panel only | `OffsetFailed` event |
| §8 Silent misdecode | Full | ⚠️ manual (forced-decode + AI chat) | automated tunability verdict |
