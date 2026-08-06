# CLAUDE.md — OpenLRLab

Browser-based WebAssembly OpenLR **diagnostic decoder and encoder** with global coverage. Rust core
→ WASM does codec + graph + A* + encoder entirely client-side; JS/MapLibre front end renders
diagnostics and a waypoint-driven encode UI. Map data is preprocessed once per source-data release
into a static PMTiles archive (R2/CDN); no live server queries at runtime. Two formats, both
decode and encode: **OpenLR binary v3** (TomTom; 11.25° bearing buckets, ~58.6 m DNP buckets) and
**TPEG-OLR / ISO 21219-22** (full precision).

**Read §2 before writing any code — several invariants fail silently (wrong output, not a crash).**

---

## 2. Critical Invariants

1. **Split at every interior junction.** Source segments/ways may have other roads attaching at
   interior positions, not just at their endpoints. The graph model is strictly node-to-node edges.
   Missing splits → junctions silently vanish; A* routes around them. Fails in dense urban areas,
   passes in sparse rural ones.

2. **Stable, deterministic ids derived from the source data — never build/row order.** Turn
   restrictions and cross-tile stitching reference segments/nodes by stable ID. A rebuild must
   produce the same ids or every restriction and boundary link breaks.

3. **A* state is `(node, incoming_segment)` from day one.** The closed/visited set is keyed on
   the pair, not the bare node. Retrofitting this later is surgery on the most correctness-critical
   code.

4. **Store geometry at full fidelity — no lossy simplification.** Bearing is derived from geometry
   over a 20 m window; the decoded path overlays a slippy basemap. Lossy simplification corrupts
   both simultaneously. The only allowed reduction is lossless removal of exactly-collinear
   vertices. Coordinate quantization (sub-meter / sub-pixel at max zoom) is the sole precision
   knob.

5. **Match window = `encoding_interval ⊕ map_tolerance τ`; τ is mandatory.** For TPEG
   (`LB == UB`) the bare interval is a point — without τ the decoder rejects every real candidate.

6. **Bearing intervals are circular (mod 360°); distance intervals are linear.** One
   `CircularInterval` type for bearing, one `LinearInterval` type for distance. Do not collapse.

7. **Cost function must stay additive and decomposable per term and per LRP/edge.** The
   diagnostic attributes score gaps to specific terms at specific LRPs; a non-additive cost
   destroys explainability.

8. **License depends on the configured source data.** OSM-derived sources (OSM directly, or any
   provider whose road-network theme derives from OSM) carry **ODbL** — attribution and
   share-alike obligations apply to the derived tile store. Verify the actual license of whichever
   source feeds a given build. See §13.

9. **A* FRC fetch is bounded by LFRCNP, not the LRP's candidate FRC tolerance.** The route
   between two LRPs may use roads down to LFRCNP, which can be much lower than the LRP's
   candidate band. Fetching only `[frc±t]` silently drops the lower-class roads connecting them.
   In v1 every tile carries all FRCs so this is automatic; it becomes a live constraint if FRC
   stratification is added.

10. **Never pick an anchor/direction with `Graph::topology_neighbors()` if nothing downstream
    re-validates it.** `topology_neighbors` (and `Graph::is_valid_node`, built on it) is
    deliberately direction-agnostic — real-world topology, ignoring one-way restrictions — and
    that's correct for structural questions like "is this a real junction". But encoding's
    `snap_point` (node/segment-hint anchoring) and Rule-4 `expand_to_valid_node` boundary
    expansion each anchor a *travel direction* with no A*/routing step afterward to catch a
    wrong-direction pick (unlike interior A* routing, which would simply fail to find a path).
    Use `Graph::outgoing_segments()` there instead — this exact bug (silently encoding a
    reference that requires travel against a one-way segment) was found and fixed three separate
    times in one session: twice in `snap_point`, once in `expand_to_valid_node`. If you're
    choosing a segment/direction at a boundary with no subsequent validator, this is almost
    certainly the wrong helper.

11. **Candidate-search tile-prefetch completeness rests on three things staying true.** Unlike
    A* interior routing (which has a `NeedsTile` retry, Invariant 3's neighbor), candidate search
    (`select_candidates`) has no fallback: it assumes every tile it could need is already loaded,
    decided entirely by `prefetch_tile_keys()` *before* any candidate search runs
    (`openlr-engine/src/lib.rs`'s own `decode()` doc comment: "graph must already contain all
    tiles needed"). This has been rigorously verified
    correct for the current architecture (see the tile-boundary correctness analysis in project
    history), but that verification leans on three specific, independently-breakable facts. Two of
    the three had live (if currently harmless) gaps, since fixed; the first is still an open,
    unaddressed constraint — treat a change to any of the three as requiring you to re-derive the
    argument, not just re-run the tests:
    - **Still open: tiles must stay geographically much larger than any configurable positional
      tolerance used in candidate matching.** `TileKey::neighborhood()` (`tile.rs`) is a fixed 3×3
      *tile* block — its geographic size is `9 × tile_edge_length²`, not a fixed number of meters.
      At today's default z12 (~10km tiles) this dwarfs even the largest configured search radius
      (500m, hard-clamped by the `ParamsPanel.jsx` UI slider; 200m in the `Permissive` preset), so
      an LRP sitting exactly on a tile corner still has its whole search circle covered by its own
      9-tile neighborhood. **Shrinking `tile_zoom` (smaller tiles) or raising the *ceiling* on any
      such tolerance erodes this margin** — push either far enough and a corner-sitting LRP's true
      search circle can reach into a tile that isn't one of its 9 neighbors, silently missing a
      real candidate. No runtime assertion enforces this margin today. Before changing default tile
      size or any positional-tolerance ceiling, add one (e.g. `tile_edge_length_m > K ×
      max_configured_radius_m` for a generous `K`) rather than relying on today's ratio staying
      accidentally huge.
    - **Fixed: prefetch and candidate search must use the same positional tolerances.**
      `prefetch_tile_keys()` sizes its per-leg corridor buffer from `candidate_search_radius_m` *as
      of the `start()` call*. `Decoder::retry_decode` (`openlr-wasm/src/lib.rs`) lets a caller
      override `DecodeParams` — including `candidate_search_radius_m` — for a fresh decode against
      the *already-loaded* graph. `retry_decode` now recomputes `prefetch_tile_keys` for the
      *merged* params first and returns `{ "needs_tiles": [[z,x,y], ...] }` instead of decoding if
      any aren't loaded yet; the LLM chat's `retry_decode` tool (`web/src/llm/tools.js`) fetches
      them via `storeActions.fetchAndLoadDecodeTiles` and retries, capped at 5 rounds. `merge_params`
      itself still applies the override with zero range validation (only the `ParamsPanel` UI slider
      clamps to 500m) — that's fine now, since an unbounded override just means more prefetched
      tiles rather than a silently incomplete search.
    - **Fixed: decode-time `zoom` must always come from the archive's own manifest, never a
      hardcoded literal standing in for it.** `web/src/App.jsx` used to silently substitute `12`
      via `manifest.tile_zoom ?? manifest.zoom ?? 12` if a manifest was ever published missing both
      fields. It now throws (surfaced through the existing setup-error UI) if neither field is
      present, rather than guessing — every subsequent `TileKey` computation depends on this
      matching the zoom the archive was actually built at, with no cross-check otherwise. The
      native (non-WASM) path never had this exposure — `PmtilesProvider` reads `min_zoom` directly
      from the PMTiles binary header (`pmtiles.rs`) rather than trusting a side-channel manifest
      field.

    Don't confuse this with `openlr-graph/src/graph.rs`'s `GRID_SCALE` comment ("fine enough to
    cover the default 30 m candidate radius in a 3×3 neighbourhood and the 200 m permissive radius
    in a ~5×5 one") — that's a *different*, much smaller (~222m) in-memory grid used only to
    accelerate `segments_near()` lookups within an already-loaded `Graph`, unrelated to PMTiles
    tile fetching. Its own query logic (`segments_near`) scales its search window from `radius_m`
    dynamically at call time, so it isn't actually at risk the way the PMTiles-tile 3×3 above is —
    but the comment's wording is easy to mistake for describing the same invariant as this one. It
    doesn't; don't let a future edit here quietly become "fixing" the wrong 3×3.

---

## 3. Architecture

```
  BUILD TIME (few times/year — separate repo: openlr-pmtiles)
  Road network source data ──▶ [openlr-pmtiles-build] ──▶ PMTiles archive ──▶ R2 + CDN

  RUNTIME (browser only, no server)
  PMTiles ──range reads──▶ [TileLoader] ──▶ [OpenLRDataProvider] ──▶ in-memory graph
                                                    │
  OpenLR string ──▶ [Codec: v3/TPEG] ──▶ unified LRP model ([LB,UB] intervals)
                                                    │
                                        [Engine: candidates + A* + validation]
                                                    │
                                        [Diagnostics + MapLibre UI]
```

All map access goes through `OpenLRDataProvider`. Primary implementation: `PmtilesProvider`.
**JS owns all I/O** — WASM operates over an in-memory tile cache that JS populates. When the
engine needs a tile it yields a tile-key request to JS; JS fetches and resumes with bytes
injected. This keeps the Rust provider synchronous and avoids async-trait across FFI.

Crates: `openlr-codec`, `openlr-graph`, `openlr-engine` (decode), `openlr-encoder` (encode),
`openlr-provider`, `openlr-wasm`.
Web frontend: `web/` (Vite + React + MapLibre GL JS).

The PMTiles builder lives in a separate repo,
[openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles) (private) — this
repo is a *consumer* of the archives it produces, not the producer. The only
contract between the two repos is the tile **format** itself (§4–5 below);
`openlr-provider`'s decoder must be updated here whenever that format changes
there. openlr-pmtiles verifies its own output against the same spec with a
self-contained test-only decoder (no dependency in either direction on this
repo's crates) — if the two ever disagree, trust the format spec, not
whichever side hasn't been updated yet.

---

## 4. Data model

### Segment (post-split, node-to-node)

| field | type | bytes | notes |
|---|---|---|---|
| start_node | u32 (tile-local) | 4 | |
| end_node | u32 (tile-local) | 4 | |
| geom_offset | u32 | 4 | vertex index into geometry pool |
| geom_len | u16 | 2 | vertex count |
| length_cm | u32 | 4 | precomputed; never re-derive from geometry |
| frc/fow/direction | u8 | 1 | packed |
| _reserved | u8 | 1 | |
| stable_id_offset | u32 | 4 | byte offset into tile string pool |
| stable_id_len | u8 | 1 | byte length in string pool (0–255) |
| _reserved | — | 7 | |

**Identity (Invariant 2):** segment identity inside a tile is its array index. The stable ID is
an opaque UTF-8 string stored in the tile's string pool and referenced by (offset, len) from the
segment record. Its meaning is entirely provider-defined: an OSM way ID, a UUID, a database key,
or any other text. The decoder and UI treat it as opaque. **Never a hash** — collisions are a
silent Invariant-2 violation.

### Node table (per tile)
`local index → { lon_e7, lat_e7, stable_id_offset u32, stable_id_len u8, flags u8 }`. Boundary
nodes (flags bit 0) require cross-tile stitching by stable ID string.

### Turn restriction table (per tile)
`(from_seg, via_node, to_seg)` — cannot live in segment records. Intra-tile: local indices.
Cross-tile: stable ID strings (from/to segment IDs referenced via string pool).

---

## 5. Tile format

Custom binary payload, not MVT. All integers little-endian.

```
Header (40 bytes)
  magic:               [u8; 4] = b"OLRL"
  version:             u8      = 3
  flags:               u8      = 0
  _pad:                [u8; 2]
  segment_count:       u32
  node_count:          u32
  restriction_count:   u32     // intra-tile
  geom_vertex_count:   u32
  xrestriction_count:  u32     // cross-tile
  string_pool_length:  u32     // byte length of string pool section
  _reserved:           [u8; 8]

Segment array:       segment_count × 32 bytes  (layout per §4)
Geometry pool:       geom_vertex_count × 8 bytes  (lon_e7: i32, lat_e7: i32)
Node table:          node_count × 28 bytes
  lon_e7: i32, lat_e7: i32,
  stable_id_offset: u32, stable_id_len: u8,
  _reserved: [u8; 11], flags: u8, _pad: [u8; 3]
Intra restrictions:  restriction_count × 16 bytes  (from_seg u32, via_node u32, to_seg u32, flags u8, pad[3])
Cross restrictions:  xrestriction_count × 16 bytes
  from_id_offset: u32, from_id_len: u8,
  via_node_local: u32,
  to_id_offset: u32, to_id_len: u8,
  flags: u8, _pad: u8
String pool:         string_pool_length bytes  (concatenated UTF-8 stable ID strings)
```

Coordinate precision: 1e-7 degrees ≈ 1 cm. `geom_offset` is a vertex index (not byte offset).
`geom_len` counts vertices. String pool offsets are byte offsets (not string indices).

**Single zoom level** (default z12, ~10 km cells). `z/x/y` is purely the addressing convention
— not a level-of-detail pyramid. Every tile holds all FRCs. Manifest records the zoom level.

---

## 6. Build pipeline

Lives entirely in the separate [openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles)
repo now — not in this one. See that repo's own docs (PreProcessing.md) for
pipeline internals, schema config, CLI reference, and open TODOs. Nothing
here should reference `pipeline/` paths or the pipeline binary directly.

---

## 7. Codec layer

```rust
// Distinct types — mod-360 wraparound logic must NEVER be applied to a linear quantity (Invariant 6).
pub struct CircularInterval { pub lb_deg: f64, pub ub_deg: f64 } // bearing; containment mod 360
pub struct LinearInterval   { pub lb: f64,     pub ub: f64 }     // meters; ordinary containment

pub struct Lrp {
    pub coord: (f64, f64),
    pub bearing: CircularInterval,
    pub frc: u8, pub fow: u8,
    pub lfrcnp: Option<u8>,                   // None on last LRP
    pub dnp: Option<LinearInterval>,          // None on last LRP
    pub pos_offset: Option<LinearInterval>,
    pub neg_offset: Option<LinearInterval>,
    pub pos_offset_raw: Option<u8>,           // raw v3 byte; derived to pos_offset once path length is known
    pub neg_offset_raw: Option<u8>,
}
```

v3 fills intervals with quantization buckets; TPEG sets `LB == UB`. All engine code is
format-agnostic past this model. `openlr-codec` serializes the same `Lrp` model back to v3
base64 and TPEG-OLR hex for the encoder (§11) — no separate encode-side codec.

---

## 8. Decode engine

See `OpenLREngine.md` for the full `crates/openlr-engine` design reference (file-by-file map,
algorithm rationale, bug-history notes) — this section is the summary.

- **Candidate selection:** project LRP coordinate onto each nearby segment polyline (nearest
  point + arc-length); compute local bearing over 20 m from that position. LRP may match anywhere
  along a segment. Start LRPs: forward 20 m bearing. Final LRP: 20 m preceding projection.
  Bidirectional segments produce two candidates; `direction` gates legality.

- **Matching (every criterion is both a hard gate and a soft penalty):**
  - *Hard gate:* value must fall within `[LB − τ, UB + τ]` (bearing) or `[LB − δ, UB + δ]`
    (distance). Outside → rejected, not penalized. Search radius and DNP window are also hard gates.
  - *Soft penalty:* zero inside `[LB, UB]`; grows with distance from nearest bound to the widened
    edge. Values inside the encoding interval are "free".
  - Total score = `positional_distance + bearing_penalty + frc_penalty + fow_penalty` (additive,
    Invariant 7). LFRCNP floor is a hard gate.

- **A\*:** state `(node, incoming_segment)` (Invariant 3). Honors `direction`, LFRCNP floor,
  turn restrictions, `max_path_search_factor` expansion cap. Runs point-on-edge → point-on-edge;
  partial first/last edges included.

- **Validation:** route length must fall within `dnp_interval ⊕ δ`. Trim with pos/neg offsets
  (both carry the same v3-bucket / TPEG-exact distinction as DNP).

---

## 9. Decode parameters

Exposed to UI; permissive defaults, tuned interactively. `DecodeParams`
(`crates/openlr-engine/src/params.rs`) is the source of truth for the full field list — don't
re-enumerate it here, it has ~17 fields and has already drifted out of sync with a hand-copied
list once. The struct's own doc comments group them; the load-bearing distinction to remember is:
- **Hard gates** (reject, don't penalize): `candidate_search_radius_m`, `max_bearing_deviation_deg`,
  `max_interior_turn_deviation_deg`, `lfrcnp_tolerance`, DNP's own interval check.
- **Soft ranking weights + penalty tables**: `distance_weight`, `bearing_weight`,
  `bearing_penalty_per_bucket`, `frc_weight` (+ `frc_penalty_table`), `fow_weight`
  (+ `fow_penalty_table`), `interior_weight`, `wrong_endpoint_weight`.
- **Search bounds**: `max_path_search_factor`, `max_astar_expansions`, `max_routing_attempts`,
  `max_candidates_per_lrp`, `max_candidate_score`.

Hard tolerances and soft penalties must stay distinct types (Invariant 7). A decode is
`(string + tolerance profile) → path`; emit both with every result for reproducibility.

---

## 10. Diagnostics (the differentiator)

See `Diagnosis.md` for the full decode-failure taxonomy (every hard-error and silent-misdecode
class, trace event fields, and which are auto-diagnosed today vs. still manual) — this section is
the summary.

1. **Stepped debugger:** candidate radius per LRP; pass/fail colours with specific reason;
   A* frontier animation; badge where path breaks.
2. **Interval visualization:** bearing wedge (wide v3 / narrow TPEG), DNP band, τ/δ halos.
3. **Desired-vs-actual explanation:**
   - Forced-decode mode is **implemented**: pin a candidate per LRP in the TracePanel (or via the
     LLM chat's `set_pinned_candidates` + `run_forced_decode` tools) and re-run A* against just
     those pins, to test directly whether a desired path is feasible and see its score table next
     to the winning path's.
   - The rest of this item is **not implemented** — still the target design, not current
     behavior:
     - Automatically diff against the chosen path at its divergence node.
     - Classify: **infeasible** (direction / turn restriction / LFRCNP / DNP / not generated /
       search limit) or **feasible-but-outscored** (attribute margin per term, per LRP).
     - **Root-cause verdict:** decoder-tunable vs. encoder-deficient.
       - Hard gates are monotonic → minimal required tolerances computed in closed form (no search).
       - Soft-ranking flip is a linear program over the weight box (cost is additive, Invariant 7).
       - Verdict is *tunable* only if some tolerance + weight vector makes the desired path the
         strict unique winner; otherwise *encoder-deficient* with proof.
       - Competitor set changes at breakpoints as gates loosen — check LP at each breakpoint.
     - Today the closest substitute is the LLM chat reasoning manually over the trace
       (see `WebFrontend.md`).

---

## 11. Encoder

`openlr-encoder` builds a `LocationReference` from a caller-supplied path through the graph —
Line and PointAlongLine only (7 other location types are UI-listed but not implemented). Reuses
`openlr-graph`/`openlr-codec` directly; no new codec, no wasm-specific logic (all of it is plain,
portable Rust — see §14 note below).

- **Line** (`line.rs`): input is an ordered path of segments plus start/end within-segment
  offsets. Rule-1 (`max_leg_m`) splits into via-point legs if any leg would otherwise exceed the
  cap; Rule-4 (`expansion.rs`) walks each boundary outward to the nearest valid node (real
  junction or dead end) if it isn't already one, composing `final_offset = original_offset +
  expansion_distance` per the whitepaper's Figure 27. `coverage.rs` re-verifies the assembled path
  with a real A*-equivalent turn-angle sweep afterward — expansion's own turn-angle stop condition
  is a heuristic escape hatch, not a substitute for this. A leg that's *still* over `max_leg_m`
  after all that (most commonly: one physical segment longer than 15km with no interior node)
  is handled by `virtual_split.rs`'s whitepaper-standard virtual-point splitting — it inserts
  intermediate LRPs along the already-verified path (preferring a valid node, then any node,
  then a mid-segment point) without re-running A*, since placing a marker along a path whose
  shortest-path correctness `coverage.rs` already established is not a new routing decision.
  For **Line**, `EncodeError::LegTooLong` now only surfaces for a degenerate `max_leg_m` config,
  not a real route. **PointAlongLine is the one exception** (see its own bullet below): it still
  raises `LegTooLong` for a genuine over-length segment, since OpenLR's PAL wire format is
  hard-fixed at exactly 2 LRPs — a PAL point on a single segment over 15km can't be represented
  at all, so there's no "next leg" for Rule-1/Rule-5 to fall back into.
  Rule-5 (an offset must be strictly less than its own bracketing leg) is also handled with the
  spec's full cascade, not an error: `pos_offset_m`/`neg_offset_m` include the entire Rule-4
  expansion distance, but `coverage.rs`'s own divergence protection can split a leg short partway
  through that expansion — so the true start/end can legitimately land past the first (or before
  the last) leg entirely. When that happens, `encode_line` drops that boundary leg, reduces the
  offset by its length, and re-applies it against the next leg in (cascading further if needed) —
  the same "place a marker along an already-verified path, no new routing decision" reasoning as
  Rule-1. Dropping a *trailing* leg is the one asymmetric case: the new final LRP has no leg of
  its own and uses the reversed bearing convention, so its bearing is recomputed (backward-facing,
  via `point_and_bearing_into_segment`'s `facing_forward` parameter) rather than reused from
  whatever attributes it had as an ordinary leg-start LRP. Exhausting every leg (the offset
  reaching or exceeding the *entire* encoded path) is asserted unreachable rather than surfaced as
  a caller-facing error — it would mean the encoder's own expansion-budget bookkeeping was
  internally inconsistent (the expansion distance is itself bounded by the same `max_leg_m` budget
  that bounds every leg), not that the caller supplied a bad route.
- **PointAlongLine** (`pal.rs`): input is one segment + an along-segment offset + orientation +
  side-of-road. No coverage-sweep step — the segment itself *is* the path, verbatim. This is
  exactly why Invariant 10 matters most here: there is nothing downstream to catch a
  wrong-direction anchor the way Line's coverage sweep or interior A* would.
- **Waypoint snapping** (`crates/openlr-encoder/src/waypoint.rs`: `snap_point`, `route_waypoints`,
  `boundary_candidates`): turns a raw map click into a graph anchor (node or mid-segment point)
  and chains legs between waypoints via the same A* the decoder's interior routing uses. This is
  real encoding logic — it originated inside `openlr-wasm` (where it was wasm-bindgen glue that
  happened to also be the only implementation) and was moved here specifically so a native caller
  doesn't have to duplicate it; see §14. `openlr-wasm`'s `Encoder` impl is now a thin adapter
  translating between this module's plain-Rust types and the JSON the JS side expects.
- **Round-trip verification**: every encode in the UI immediately decodes its own output (both
  v3 and TPEG) through the ordinary decoder — this *is* a real decode, so it drives the same
  Segments/Trace/Replay panels the decode side already has, unmodified.
- **Diagnostics** (`diagnose.rs`): `diagnose_connection` distinguishes genuine disconnection from
  being blocked specifically by the turn-angle gate; `check_boundary_expansion` replays Rule-4
  expansion in isolation. Both are exposed as LLM chat tools (`web/src/llm/tools.js`,
  `SYSTEM_PROMPT.md`) for the same reason the decode side's trace-drilldown tools are.

---

## 13. Licensing & attribution (non-negotiable)

Licensing depends entirely on the configured source data — verified at build time in
[openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles), not here. Sources derived from OSM
(OSM directly, or any provider whose road-network theme is OSM-derived, e.g. Overture) carry
**ODbL**: the derived tile store and all served output must preserve attribution and honour
share-alike obligations. Document exact attribution text before public release.

---

## 14. Native (non-wasm) use — unblocked, no CLI binary written yet

`openlr-codec`/`openlr-graph`/`openlr-engine`/`openlr-encoder`/`openlr-provider` have no
`wasm-bindgen`/`wasm32` dependency — they're plain, portable Rust. `openlr-provider` already has a
native `PmtilesReader` (`pmtiles.rs`, `std::fs::File`-based, no HTTP) alongside the byte-injection
path the web app uses. The waypoint-snapping/routing logic (`snap_point`, `route_waypoints`,
`boundary_candidates` — see §11) used to be the one exception, living only in `openlr-wasm` as
wasm-bindgen glue that happened to also be the sole implementation; it now lives in
`openlr-encoder/src/waypoint.rs` as plain Rust with its own unit tests, so it's no longer a
blocker. A native CLI/HTTP binary crate (batch decode/encode against a local `.pmtiles` archive,
or a remote one — see the discussion of the `pmtiles` crate's HTTP-range backend as an alternative
to `PmtilesReader` for that case) is therefore genuinely just plumbing now: argument parsing,
wiring `DecodeParams`/profile resolution, and a batch loop. Not written yet.

---

## 15. Agent conventions

- Prefer small, well-typed crates with clear boundaries. Codec must not leak format specifics past
  the unified LRP model; engine must not know which provider backs it.
- Keep cost function additive/decomposable; keep hard tolerances and soft penalties separate types.
- This repo has no pipeline/tile-building code and no `fixtures/` corpus of its own — those live in
  openlr-pmtiles. Don't reintroduce `pipeline/`-shaped code or dependencies here; a tile-format
  change belongs there first, then propagates to `openlr-provider`'s decoder in this repo.
- When a decision is genuinely open, state the assumption inline and proceed; never silently
  violate a Critical Invariant to make something compile.
- **Docs and the onboarding tour drift silently — treat that as a bug, not cosmetic.** Nothing
  fails a build when `README.md`/`WebFrontend.md`, `web/src/docs/userGuide.md` (Documentation
  panel), or `web/src/components/OnboardingTour.jsx` fall out of sync with the actual UI/behavior —
  a stale tutorial or a tour spotlight pointing at nothing just looks like the tool doesn't work.
  Whenever a change touches user-facing behavior:
  - Adding/renaming/removing a menu item, panel, or decode/encode parameter → update
    `userGuideMd` and/or the `HELP` object in `refFormat.js` (the single source shared by the `?`
    tooltips and the Documentation panel's parameter reference — edit it once, not both places).
  - Renaming or removing a DOM element/class that carries a `data-tour` / `data-tour-solo`
    attribute, or restructuring where one lives → grep for that selector in
    `OnboardingTour.jsx`'s `STEPS` array first. `unionRect()` degrades silently on a no-match
    selector (empty spotlight, not an error), so a broken tour step won't surface on its own.
  - Architecture/setup changes (new crate, changed build/deploy steps, new invariant) → update
    the relevant doc in this repo (`README.md`, `WebFrontend.md`, or this file).
