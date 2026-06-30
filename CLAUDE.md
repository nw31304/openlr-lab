# CLAUDE.md — OpenLRLens

Browser-based WebAssembly OpenLR **diagnostic decoder** with global coverage. Rust core → WASM
does codec + graph + A* entirely client-side; JS/MapLibre front end renders diagnostics. Map data
is preprocessed once per Overture release into a static PMTiles archive (R2/CDN); no live server
queries at runtime. Two decode formats: **OpenLR binary v3** (TomTom; 11.25° bearing buckets,
~58.6 m DNP buckets) and **TPEG-OLR / ISO 21219-22** (full precision). Encoder is stubbed.

**Read §2 before writing any code — several invariants fail silently (wrong output, not a crash).**

---

## 2. Critical Invariants

1. **Split at every interior connector.** Overture segments may have connectors at interior
   positions. The graph model is strictly node-to-node edges. Missing splits → junctions silently
   vanish; A* routes around them. Fails in dense urban areas, passes in sparse rural ones.

2. **Stable, deterministic ids derived from GERS — never build/row order.** Turn restrictions and
   cross-tile stitching reference segments/nodes by id. A rebuild must produce the same ids or
   every restriction and boundary link breaks.

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

8. **License: ODbL.** Overture transportation is OSM-derived. Attribution (OSM + Overture) and
   share-alike obligations apply to the derived tile store. See §13.

9. **A* FRC fetch is bounded by LFRCNP, not the LRP's candidate FRC tolerance.** The route
   between two LRPs may use roads down to LFRCNP, which can be much lower than the LRP's
   candidate band. Fetching only `[frc±t]` silently drops connectors. In v1 every tile carries
   all FRCs so this is automatic; it becomes a live constraint if FRC stratification is added.

---

## 3. Architecture

```
  BUILD TIME (few times/year)
  Overture parquet ──▶ [pipeline/] ──▶ PMTiles archive ──▶ R2 + CDN

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

Crates: `openlr-codec`, `openlr-graph`, `openlr-engine`, `openlr-provider`, `openlr-wasm`.
Pipeline binary: `pipeline/`. Web frontend: `web/` (Vite + React + MapLibre GL JS).

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
| flags | u8 | 1 | |
| reserved | — | 12 | |

**Identity (Invariant 2):** segment identity inside a tile is its array index; global stable id
(full GERS UUID, 16 bytes) lives in side tables (`local index → GERS id`). Cross-tile references
use the global GERS id. **Never a hash** — collisions are a silent Invariant-2 violation.

**`stable_id` byte layout** (OSM tiles): bytes 0–7 = source integer (i64 LE), bytes 8–11 =
split index (u32 LE), bytes 12–15 = 0. Produces `source_key` strings like `"372358612-1"`.
Full GERS UUIDs have non-zero bytes 12–15. Decoded by `segment_source_key()` in `openlr-wasm`.

### Node table (per tile)
`local index → { lon_e7, lat_e7, gers_id[16], flags }`. Boundary nodes (flags bit 0) require
cross-tile stitching by GERS id.

### Turn restriction table (per tile)
`(from_seg, via_node, to_seg)` — cannot live in segment records. Intra-tile: local indices.
Cross-tile: global GERS ids.

---

## 5. Tile format

Custom binary payload, not MVT. All integers little-endian.

```
Header (40 bytes)
  magic:              [u8; 4] = b"OLRL"
  version:            u8      = 1
  flags:              u8      = 0
  _pad:               [u8; 2]
  segment_count:      u32
  node_count:         u32
  restriction_count:  u32     // intra-tile
  geom_vertex_count:  u32
  xrestriction_count: u32     // cross-tile
  _reserved:          [u8; 12]

Segment array:       segment_count × 32 bytes  (layout per §4)
Geometry pool:       geom_vertex_count × 8 bytes  (lon_e7: i32, lat_e7: i32)
Node table:          node_count × 28 bytes  (lon_e7, lat_e7, gers_id[16], flags u8, pad[3])
Intra restrictions:  restriction_count × 16 bytes  (from_seg u32, via_node u32, to_seg u32, flags u8, pad[3])
Cross restrictions:  xrestriction_count × 40 bytes (from_gers[16], via_node_local u32, to_gers[16], flags u8, pad[3])
```

Coordinate precision: 1e-7 degrees ≈ 1 cm. `geom_offset` is a vertex index (not byte offset).
`geom_len` counts vertices.

**Single zoom level** (default z12, ~10 km cells). `z/x/y` is purely the addressing convention
— not a level-of-detail pyramid. Every tile holds all FRCs. Manifest records the zoom level.

---

## 6. Build pipeline — COMPLETE

All steps implemented and verified on NZ (1.5 M edges, 4 680 tiles at z12). Schema mapping is
**external TOML** (`pipeline/schema/overture-default.toml`), not hardcoded — pass `--schema`
to override when Overture revises its taxonomy.

**Remaining TODOs:**
- Restrictions are 0 in output — `prohibited_transitions` schema not yet validated against live Overture data
- `write_tiles` in tile.rs buffers all payloads before writing — streaming writer needed for world-scale
- Scale to full planet

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
    pub lfrcnp: u8,
    pub dnp: Option<LinearInterval>,          // None on last LRP
    pub pos_offset: Option<LinearInterval>,
    pub neg_offset: Option<LinearInterval>,
}
```

v3 fills intervals with quantization buckets; TPEG sets `LB == UB`. All engine code is
format-agnostic past this model. Encoder is stubbed behind `OpenLrEncoder` trait.

---

## 8. Decode engine

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

Exposed to UI; permissive defaults, tuned interactively:
- `candidate_search_radius_m` — positional tolerance
- `bearing_tolerance_deg` (τ) — map-divergence term; widens the encoding interval
- `dnp_tolerance_pct` (δ) — percentage tolerance on DNP; combined with absolute v3 bucket
- `frc_weight_penalty`, `fow_weight_penalty` — soft ranking weights
- `max_path_search_factor` — A* expansion cap
- `lfrcnp_tolerance` — additional LFRCNP slack

Hard tolerances and soft penalties must stay distinct types. A decode is
`(string + tolerance profile) → path`; emit both with every result for reproducibility.

---

## 10. Diagnostics (the differentiator)

1. **Stepped debugger:** candidate radius per LRP; pass/fail colours with specific reason;
   A* frontier animation; badge where path breaks.
2. **Interval visualization:** bearing wedge (wide v3 / narrow TPEG), DNP band, τ/δ halos.
3. **Desired-vs-actual explanation:**
   - Run user's desired path through the same feasibility + cost functions (forced-decode mode).
   - Diff against chosen path at divergence node.
   - Classify: **infeasible** (direction / turn restriction / LFRCNP / DNP / not generated /
     search limit) or **feasible-but-outscored** (attribute margin per term, per LRP).
   - **Root-cause verdict:** decoder-tunable vs. encoder-deficient.
     - Hard gates are monotonic → minimal required tolerances computed in closed form (no search).
     - Soft-ranking flip is a linear program over the weight box (cost is additive, Invariant 7).
     - Verdict is *tunable* only if some tolerance + weight vector makes the desired path the
       strict unique winner; otherwise *encoder-deficient* with proof.
     - Competitor set changes at breakpoints as gates loosen — check LP at each breakpoint.

---

## 13. Licensing & attribution (non-negotiable)

Overture transportation is OSM-derived, carried under **ODbL**. The derived tile store and all
served output must preserve attribution (OSM + Overture) and honour share-alike obligations.
Document exact attribution text before public release.

---

## 15. Agent conventions

- Prefer small, well-typed crates with clear boundaries. Codec must not leak format specifics past
  the unified LRP model; engine must not know which provider backs it.
- Keep cost function additive/decomposable; keep hard tolerances and soft penalties separate types.
- Maintain the `fixtures/` regression corpus; add a fixture whenever a decode behaviour is pinned.
- When a decision is genuinely open, state the assumption inline and proceed; never silently
  violate a Critical Invariant to make something compile.
