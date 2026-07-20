# OpenLRLab — Web Frontend

This document describes the web frontend: its architecture, component tree, state
management, WASM decode/encode protocol, MapLibre layer model, and the tile geometry
caching pattern. It is the canonical reference for resuming frontend work after a
context gap. §1–19 cover the original decode-only app; §20 covers encode mode, added
later — read §20 alongside §3/§5 (state and Map.jsx), not as a standalone add-on.

---

## 1. Overview

The frontend is a **Vite + React SPA** that runs entirely client-side. There is no
backend server — the map data comes from range-read HTTP requests against a PMTiles
archive, and decoding/encoding both run inside a WASM module compiled from the Rust
core (`Decoder` and `Encoder` structs — see §4 and §20).

```
Browser
  App.jsx           — startup, WASM init, tile base URL from ?tiles= param
    MenuBar.jsx     — decode/encode mode toggle, location-type dropdown, panel toggles
    TopBar.jsx      — OpenLR string input, preset, toggles, Decode button (decode mode)
    ParamsPanel.jsx — all DecodeParams fields; FRC/FOW penalty tables
    Map.jsx         — MapLibre GL JS canvas; GeoJSON sources/layers for both modes
    ResultPanel.jsx — decoded segment list; click-to-highlight (decode mode)
    EncodeResultPanel.jsx — v3/TPEG output, verify-decode glance badge (encode mode)
    TracePanel.jsx  — full trace: candidates, A*, DNP, offsets, result (both modes —
                      reads decodeResult or verifyResult depending on mode)
    ReplayPanel.jsx — step-by-step A* replay (both modes)
    BottomBar.jsx   — status line
    LlmChatPanel.jsx / LlmSettingsPanel.jsx — AI assistant, both modes
    DecodeToast.jsx — transient result/error banner
```

Development entry: `web/` directory, `npm run dev` (Vite dev server on :5173, tile
server on :5176 with HTTP 206 range support).

---

## 2. Startup sequence (`App.jsx`)

1. Parse `?tiles=<base>` from the URL query string. In dev, prepend
   `http://localhost:5176` unless the value is an absolute URL.
2. Fetch `{base}/manifest.json` → `{ archive, tile_zoom }`.
3. Instantiate `PMTiles({base}/{archive})`.
4. Call `initWasm()` (`wasm.js`) → WASM module `decoder`.
5. Call `setPmtiles(pmtiles)`, `setDecoder(decoder)`, `setZoom(manifest.tile_zoom)` on
   the module-level refs in `store.js`.
6. Set `ready = true` → `MapView` receives `tilesBase` prop and begins loading.

---

## 3. State management (`store.js`)

Uses **Zustand**. All shared UI state lives here. The fields below are decode-side;
encode-side fields (`mode`, `waypoints`, `liveRoute`, `encodeResult`, `verifyResult`, …)
are listed in §20.2.

### Store fields

| Field | Type | Description |
|---|---|---|
| `openlrString` | string | Raw input string |
| `preset` | 'Permissive' \| 'Default' \| 'Strict' | Active preset name |
| `params` | `DecodeParams` object | All decode parameters |
| `showParams` | bool | ParamsPanel visible |
| `showTrace` | bool | TracePanel visible |
| `showSegmentLayer` | bool | OLR segment FRC layer visible |
| `showReplay` | bool | ReplayPanel visible |
| `showLlmSettings` | bool | LLM settings panel visible |
| `decoding` | bool | Decode in progress |
| `decodeResult` | object \| null | Last decode result from WASM |
| `highlightedSegment` | `{tile, local_index}` \| null | Segment highlighted from ResultPanel |
| `requestedInfoSegment` | `{tile, local_index}` \| null | Segment for which popup info was explicitly requested (ResultPanel row click) |
| `traceHighlightSegIds` | `number[]` \| null | Segment IDs to highlight from TracePanel |
| `traceLrpFocus` | `{lon, lat, index, …, _tick}` \| null | LRP to pan to (with `_tick` to allow re-click) |
| `candidatePopup` | object \| null | Data for the candidate evaluation popup opened from the TracePanel (see §10) |
| `replaySteps` | `Step[]` | Pre-built display steps from `buildReplaySteps()` |
| `replayStats` | `{maxG, totalNodes, phases}` \| null | Summary stats for the replay (used for colour normalisation and timeline phases) |
| `replayStep` | number | Current display step index (0-based) |
| `llmConfig` | object \| null | LLM provider config `{provider, apiKey, model}`; persisted to localStorage |
| `llmChatOpen` | bool | LLM chat panel visible |
| `llmMessages` | `{role, content, error?}[]` | LLM chat message history for the current session |
| `llmLoading` | bool | LLM request currently in flight |

### Zustand `persist` and the `merge` function

Params are persisted to `localStorage` under the key `openlrlab-settings`. The `persist`
middleware uses a custom `merge` function so that new fields added to `PRESETS.Default` in
a future release survive localStorage upgrades without reverting to `undefined`:

```js
merge: (persisted, current) => ({
  ...current,
  ...persisted,
  params: { ...current.params, ...persisted.params },
}),
```

This deep-merges `params`: persisted values win, but any field present in the current default
and absent from the persisted object (e.g. a newly added field) falls back to the current
default rather than being silently lost.

### Three module-level caches (not Zustand)

These are plain `Map` instances at module scope in `store.js`, rebuilt on every decode:

| Variable | Key | Value | Purpose |
|---|---|---|---|
| `_tileGeomCache` | `tile_key` (`"z/x/y"`) | `GeoJSON Feature[]` | All decoded-tile features for fallback lookup |
| `_segIdToTile` | `segment_id` (number) | `{tile_key, local_index}` | Bridge from engine segment ID to tile index |
| `_segGeomCache` | `segment_id` (number) | `GeoJSON Feature` | Direct O(1) lookup used by trace highlight |

These are exported via getter functions (`getSegGeomCache`, `getSegIdToTile`,
`getTileGeomCache`) and read in `Map.jsx` effects.

---

## 4. WASM decode protocol

The WASM module is a three-step call sequence managed in `store.js::runDecode()`:

```js
// Step 1 — parse OpenLR string, compute required tiles
const startResult = JSON.parse(decoder.start(openlrString, paramsJson, zoom));
// startResult.tiles = [[z, x, y], ...]

// Step 2 — fetch and load each tile
for each [z, x, y] of startResult.tiles:
  const bytes = await pmtiles.getZxy(z, x, y);
  decoder.load_tile(z, x, y, new Uint8Array(bytes.data));

// Step 3 — run decode
const result = JSON.parse(decoder.decode());
```

After step 2, the three caches are populated:
- `_tileGeomCache`: from `decodeTile(res.data, z, x, y).features` (JS-side tile decode)
- `_segIdToTile` and `_segGeomCache`: from `decoder.all_segment_tile_mappings()`, an
  O(n) WASM bulk export of `[[segId, z, x, y, local_index], …]`.

### Important: `isStyleLoaded()` must NOT guard custom-source effects

MapLibre's `isStyleLoaded()` returns `false` while the basemap background tiles are
loading — this can be long after the custom sources are fully set up. Effects that
operate on custom sources (e.g. `trace-segment`, `highlighted-segment`) must guard with
`if (!map.getSource('source-name')) return;` rather than `if (!map.isStyleLoaded())`.

---

## 5. MapLibre sources and layers (`Map.jsx`)

GeoJSON sources added on the `map.on('load')` callback, split into permanent and replay groups:

**Permanent sources:**

| Source | Layer(s) | Purpose |
|---|---|---|
| `olr-segments` | `olr-frc0` … `olr-frc7`, `olr-highlight` | All road segments, FRC-coloured; toggled by Segs button |
| `decoded-path` | `decoded-path-line`, `decoded-path-arrow` | Solid decoded path (cyan) with dense dark-navy direction triangles (SDF, 18px spacing, white halo) |
| `offset-uncertainty` | `offset-uncertainty-line` | Dashed darker-cyan line for v3 offset uncertainty zones at path head/tail |
| `lrp-markers` | `lrp-markers-circle` | LRP point markers (purple circles) |
| `highlighted-segment` | `highlighted-segment-halo`, `highlighted-segment-line` | Segment highlighted from ResultPanel or TracePanel; animated pulse halo |
| `trace-segment` | `trace-segment-halo`, `trace-segment-line`, `trace-segment-arrow` | Highlight driven by TracePanel segment buttons. When a candidate popup is active: line colour = green (accepted) or red (rejected); arrow layer becomes visible with matching colour + white halo; coordinates reversed for Backward traversal |

**Replay sources** (all toggled via `visibility` when `showReplay` changes):

| Source | Layer(s) | Purpose |
|---|---|---|
| `replay-radius` | `replay-radius-fill`, `replay-radius-line` | LRP candidate search radius ring (dashed purple) |
| `replay-route` | `replay-route-line` | Found leg route (gold line); pulsed for 3 s when `route_found` fires |
| `replay-candidates` | `replay-candidates-line`, `replay-candidates-arrow` | Candidate segments as coloured LineStrings (green = accepted, red = rejected, bright green = winner) with direction triangles. Coordinates reversed for Backward traversal so arrows point the correct way. See §15 for colour key |
| `replay-cloud` | `replay-cloud-circle` | A* expanded-node cloud; colour by `g_m/maxG` ramp (blue→yellow→red) |
| `replay-frontier` | `replay-frontier-circle` | Latest `FRONTIER_SIZE=25` A* nodes; sinusoidal pulse animation |
| `replay-leg` | `replay-leg-from`, `replay-leg-to` | Green (from) / red (to) leg endpoint markers |
| `replay-flash` | `replay-flash-ring` | Sonar-ping ring on the newest A* node; expands and fades over 2 s |

### Offset uncertainty visualization

The decoded path uses three geometrically non-overlapping segments to visualize v3 offset
uncertainty:

1. **Solid path** (`decoded-path`): the conservative result, trimmed at `pos_offset_ub`
   forward and `neg_offset_ub` backward. This is the portion definitely inside the reference.
2. **Positive cap** (`decoded-path-uncertainty-pos`): dashed line from `pos_offset_lb` to
   `pos_offset_ub` at the path head — the uncertain "start" zone.
3. **Negative cap** (`decoded-path-uncertainty-neg`): dashed line from `neg_offset_ub` to
   `neg_offset_lb` at the path tail — the uncertain "end" zone.

The dashes use `line-dasharray: [1, 0.5]` (dense short dashes) at `line-width: 2`.

The three segments are geometrically non-overlapping by construction: the solid path starts
where the positive cap ends (`pos_offset_ub`), and ends where the negative cap starts
(`neg_offset_ub`). The WASM result carries both `wkt` (midpoint trim, for the midpoint
estimate) and `conservative_wkt` (UB trim, for the solid portion). When no offsets are
present, only the solid `decoded-path` source is populated.

### `olr-segments` — tile inspector layer

Populated by `loadVisibleTiles()`, which runs on map load, `moveend`, and `zoomend`.
It maintains its own `tileCacheRef` (a `Map` keyed by tile key) and its own PMTiles
reader (`pmtilesRef`) — separate from the decode-time reader in `store.js`. Both
readers share the same underlying HTTP cache via the browser.

The segment layer is hidden (`visibility: 'none'`) until the user clicks "Segs". It
becomes visible once `showSegmentLayer` is true and `zoom ≥ MIN_LOAD_ZOOM (10)`.

### Highlight sync effects

Key React effects in `Map.jsx` that react to Zustand state:

1. **`[highlightedSegment, traceHighlightSegIds]`**: updates `highlighted-segment`
   source. Reads decode result via `decodeResultRef` (a ref, not a dependency) to
   avoid racing with the decode-result effect. Starts/cancels the sinusoidal halo pulse
   animation via `requestAnimationFrame`.

2. **`[traceHighlightSegIds]`**: updates `trace-segment` source using the three
   module-level caches. Primary path: `segGeomCache.get(segId)`. Fallback: two-step
   lookup via `segIdToTile` + `tileGeomCache`. When a candidate popup is active and its
   traversal is `Backward`, reverses the feature coordinates before setting the source
   so `trace-segment-arrow` chevrons point in the traversal direction. Suppresses the
   segment info popup when a candidate popup is already being shown.

3. **`[candidatePopup]`**: updates `candidatePopupRef.current` (a ref used inside
   effect 2 without adding it as a dependency). Calls `map.setPaintProperty` on
   `trace-segment-line` (green/red colour) and toggles `trace-segment-arrow` visibility.
   Applies a MapLibre filter to `replay-candidates-arrow` restricting arrows to the
   selected candidate's `segment_id` + `traversal`, preventing overlapping
   opposite-direction arrows when both traversals of a bidirectional segment are in the
   candidate list. Clears the filter when the popup closes.

4. **`[traceLrpFocus]`**: calls `map.flyTo()` to pan to the LRP and shows the LRP
   info popup. Clears `traceLrpFocus` after acting so the same LRP can be clicked again.

5. **`[showSegmentLayer]`**: toggles visibility of `olr-frc*` and `olr-highlight` layers.

6. **`[decodeResult]`**: populates `decoded-path` and `lrp-markers` sources; calls
   `map.fitBounds()` (deferred one frame via `requestAnimationFrame` to allow `setData`
   to process first).

---

## 6. Tile decoder (`tileDecoder.js`)

Decodes the custom OLRL v3 binary tile payload into GeoJSON features. Used both by
the segment-inspector layer (in `Map.jsx`) and at decode time (in `store.js`) to build
`_tileGeomCache`.

### Binary layout read by the JS decoder

```
Header              40 bytes
Segment array       segment_count × 32 bytes
Geometry pool       geom_vertex_count × 8 bytes  (lon_e7: i32, lat_e7: i32, LE)
Node table          node_count × 28 bytes
Intra restrictions  restriction_count × 16 bytes
Cross restrictions  xrestriction_count × 16 bytes
String pool         string_pool_length bytes (UTF-8 stable IDs)
```

Each segment record carries `(stable_id_offset u32, stable_id_len u8)` at bytes [20–24]
pointing into the string pool. Each node record carries the same at bytes [8–12].

Each feature's `properties` includes: `frc`, `frc_name`, `fow`, `fow_name`, `direction`,
`length_m`, `tile` (`"z/x/y"`), `local_index` (segment array index), `stable_id`
(opaque provider-defined text key — e.g. `"372358612-1"` for OSM, a UUID for others).

The `local_index` property is the canonical join key between JS feature caches and
WASM segment references.

---

## 7. TopBar (`TopBar.jsx`)

Contains:
- **OpenLR input**: text input; Enter key triggers decode
- **Gear menu** (`⚙`): dropdown with toggle rows for Road segments, Trace panel, and Replay; plus a **Trace level** button group (Off / Summary / Full) that sets `params.trace_level`; plus Parameters… and Reset to defaults actions
- **▶ Replay button**: appears only when `replaySteps.length > 0`; toggles `showReplay`
- **Decode button**: calls `runDecode()`; disabled while `decoding` is true

`trace_level` must be set to **Full** before decoding to get `AStarNodeExpanded` events, which are required for A\* visualisation in the replay. Summary level records candidates and routing outcomes only.

---

## 8. Params panel (`ParamsPanel.jsx`)

Shows all fields from `DecodeParams` as labelled inputs, including:
- Spatial: `candidate_search_radius_m`, `snap_to_endpoint_threshold_m`
- Weights: `distance_weight`, `bearing_weight`, `bearing_penalty_per_bucket`,
  `frc_weight`, `fow_weight`, `interior_weight`, `wrong_endpoint_weight`
- Hard gates: `max_bearing_deviation_deg`, `max_candidate_score`
- Routing: `max_candidates_per_lrp`, `dnp_tolerance_pct`, `max_path_search_factor`,
  `max_astar_expansions`, `lfrcnp_tolerance`
- Trace: `trace_level` (Off / Summary / Full)

Also renders two editable 8×8 penalty tables (`frc_penalty_table`, `fow_penalty_table`).
Changes call `setParam(key, value)` or `setTableCell(tableKey, row, col, value)` on the
store. Mutating any cell clears the preset name (shows "Custom").

---

## 9. Result panel (`ResultPanel.jsx`)

Shown after a successful decode. Draggable (via `useDraggable`). Shifts left when
the trace panel is open (`right: '416px'` unless dragged).

Lists all decoded segments with Seg ID, FRC, FOW, and OSM way link. Clicking a row
calls `setHighlightedSegment({tile, local_index})`, which the highlight-sync effect
in `Map.jsx` turns into a map highlight + camera pan.

---

## 10. Trace panel (`TracePanel.jsx`)

Shown when the `⚡` button is active. Draggable. Shows a structured view of the
`decodeResult.trace.events` array.

### Trace on decode failure

When a decode fails, the WASM module now includes the partial trace in the error response
(via `DecodeFailure` in the engine). The TracePanel renders normally even on failure — the
user can inspect which candidates were found, which were rejected, and why. The Copy JSON
button is enabled whenever `decodeResult` is non-null (success or failure).

### Event parsing (`parseTraceEvents`)

Partitions the flat event list into:
- `candidates[lrp_idx]` — `{ searchStart, evaluated[], ranked }` per LRP
- `routing[leg]` — `{ start, astarNodes[], astarSkipped[], result, dnp }` per leg
- `offsets[]` — offset application events
- `decodeComplete` — terminal outcome

### Sections rendered

- **Codec**: input string + LRP table (lon, lat, FRC, FOW, bearing, LFRCNP). Clicking
  a row calls `setTraceLrpFocus({…, index})` → map pans to LRP, shows LRP popup.

- **Candidates — LRP N**: accepted candidates ranked by score (lower = better). The
  top row has the `tp-best-row` style. Each candidate's Seg cell is a `SegBtn`.
  Below the accepted table, a collapsible `RejectedTable` shows all rejected candidates
  (expandable via "▸ N rejected" toggle). Each row shows: segment ID (`SegBtn`),
  direction (Fwd/Bwd), distance, bearing, and a colour-coded gate-failure pill:
  - `tp-gate-bearing` (amber): bearing deviation exceeded `max_bearing_deviation_deg`
  - `tp-gate-radius` (yellow): outside `candidate_search_radius_m`
  - `tp-gate-score` (purple): total score exceeded `max_candidate_score`
  - `tp-gate-other` (grey): degenerate geometry (`FailDirection`)

- **Routing — Leg N**: From/To segment buttons; path highlight button `[N segs]`;
  DNP check result. For direct-match legs (both LRPs on the same segment, DNP = 0),
  shows the segment from the top candidates with "same-segment match" note instead of
  From/To. If `trace_level = Full`, shows A* node expansion table (capped at 200
  rows) and skipped-edge details.

- **Offsets**: positive/negative trim values.

- **Result**: success/failure, segment count, offset amounts, WKT preview.
  The "Copy WKT" button uses `conservative_wkt ?? wkt` — the conservative (UB-trimmed)
  path when offsets are present, otherwise the midpoint-trimmed path.
  The "Copy JSON" button copies the full `decodeResult` (including partial trace on
  failure) and is enabled whenever `decodeResult` is non-null.

### `SegBtn`

Clickable badge that calls `setTraceHighlight([segId])`, then optionally calls an
`onSelect` callback. The `e.stopPropagation()` call is required to prevent the section
collapse from intercepting the click. `setTraceHighlight` in the store sets
`traceHighlightSegIds`, which triggers the trace highlight effect in `Map.jsx`.

### Candidate evaluation popup (`candidatePopup` store state)

Clicking a candidate row (accepted or rejected) in the TracePanel calls both
`setTraceHighlight([segId])` and `setCandidatePopup(buildCandPopup(...))`. The popup
is a draggable overlay (`cand-panel` CSS class) anchored at the candidate snap point.

`buildCandPopup(segId, lrpIdx, traversal, ctype, winner, snapPt, projection, score, verdict)`
constructs the popup data object from the trace event fields plus geometry-cache lookups:

| Field | Source |
|---|---|
| `segment_id`, `source_id` | engine ID + `segGeomCache` properties |
| `traversal` | `'Forward'` \| `'Backward'` |
| `ctype` | `'accepted'` \| `'bearing'` \| `'radius'` \| `'score'` \| `'direction'` |
| `winner` | true for the chosen leg endpoint |
| `snap_lon`, `snap_lat` | projection snap point (used to anchor the popup on the map) |
| `distance_m`, `arc_offset_m`, `bearing_deg` | from `projection` |
| `score_total`, `score_distance`, `score_bearing`, `score_frc`, `score_fow`, `score_wrong_ep`, `score_interior` | from `score` |
| `frc`, `fow`, `frc_name`, `fow_name`, `direction`, `length_m` | from `segGeomCache` feature properties |

The popup body (`CandidatePopupBody`) shows three sections:
- **Result**: Accepted / Rejected with ★ badge for winners
- **Segment**: Key, Traversal, FRC N (name), FOW N (name), Direction, Length
- **Projection**: Dist from LRP, Arc offset, Bearing
- **Score**: all 7 components (lower = better); only for accepted candidates
- **Gate failure**: human-readable reason; only for rejected candidates

`clearCandidatePopup()` is called on new decodes and when the popup is closed. The
`candidatePopupRef` (a `useRef`) mirrors the store state so that the
`traceHighlightSegIds` effect can read traversal info without adding `candidatePopup`
as a dependency and causing unwanted re-runs.

### Direct-match detection

```js
const isDirect = !start && dnp?.actual_m === 0;
```

When both LRPs of a leg project onto the same segment, the engine emits a `RouteFound`
with an empty path and DNP = 0, but no `RouteSearchStarted`. The `isDirect` condition
handles this: `!start` means no `RouteSearchStarted` was emitted; `dnp?.actual_m === 0`
confirms the zero-length direct match. The segment IDs come from the top accepted
candidates for the two surrounding LRPs.

---

## 11. `useDraggable` hook (`hooks.js`)

Makes a panel draggable by its header element.

```js
const { pos, onMouseDown } = useDraggable(panelRef);
// pos = null (use CSS defaults) | { left, top } (panel has been dragged)
// onMouseDown = attach to the drag handle element's onMouseDown prop
```

Internally: `onMouseDown` records the initial panel rect and mouse position into
`dragState.current`; document-level `mousemove`/`mouseup` listeners (added in a
`useEffect`) drive `setPos()` and clean up on mouse-up. The listeners are added
once on mount and removed on unmount.

---

## 12. DNP display in TracePanel

The DNP row clamps the lower bound to zero before display:

```jsx
DNP {fmtM(dnp.actual_m)} ∈ [{fmtM(Math.max(0, dnp.interval?.lb ?? 0))}, {fmtM(dnp.interval?.ub)}]
```

This prevents a visual `-29.3 m` lower bound for v3 bucket 0 (lb = 0, delta applied
symmetrically, but the semantically valid lower bound cannot be negative). The engine's
`validate_dnp` uses `window = dnp.widen(delta)` where `delta = path_length_m ×
dnp_tolerance_pct`; for a zero-length path, delta = 0 and the window is exactly
`[0.0, 58.6]`, so clamping is not needed in that case — the clamping is only a display
guard for intermediate trace events where the interval might differ.

---

## 13. Decode replay system

### Overview

After a decode the `DecodeTrace.events` array is converted into a visual step-by-step replay. Two new files implement this:

- **`replayEngine.js`** — pure transformation logic (no MapLibre dependency)
- **`ReplayPanel.jsx`** — the panel UI: ◀ / ▶ buttons, step counter, scrubable timeline

The trace engine emits events; the replay engine converts them into *display steps* and accumulates a mutable *visual state* that `Map.jsx` maps to GeoJSON sources.

### `replayEngine.js`

**`buildReplaySteps(events)`** converts the flat `events` array into a `{ steps, stats }` pair:
- One step per `CandidateSearchStarted`, `CandidatesRanked`, `RouteSearchStarted`, and `RouteFound`/`RouteFailed`/`DnpChecked`/`OffsetApplied`/`DecodeComplete` event
- `AStarNodeExpanded` events are grouped into batches of `ASTAR_BATCH=1` (one node per display step)
- `CandidateEvaluated` and `AStarEdgeSkipped` events are discarded (summary data only)
- `stats.maxG` normalises A\* node colours; `stats.phases` drives the timeline colour strips

**`emptyState()` / `applyStep(state, step, maxG)`** — incrementally mutate the visual state object. `applyStep` is O(1) per call; forward stepping is O(1) total. Backward jumps fall back to `computeVisualState` (O(N) full replay from step 0).

**Visual state fields:**

| Field | Purpose |
|---|---|
| `searchRadius` | Current LRP search circle: `{ lon, lat, radiusM, lrpIdx }` |
| `candidates` | All candidate snap points; each carries full projection + score detail |
| `astarNodes` | Accumulated A\* expanded nodes; each has a pre-computed `color` |
| `frontier` | Last 25 A\* nodes (for the pulsing frontier layer) |
| `currentLeg` | Active leg: `{ leg, fromPt, toPt, fromSegId, toSegId }` |
| `routeSegIds` | Segment IDs of the most recently found route |

**Candidate `winner` flag**: set to `true` on `route_search_started` for the candidates whose `segment_id` matches `step.from.segment_id` or `step.to.segment_id`. Winners render with a white ring and larger radius in the `replay-candidates-circle` layer.

**`stateToGeoJSON(state, geomLookup)`** converts visual state to `{ radiusFC, candFC, cloudFC, frontierFC, legFC }`. The `geomLookup` parameter is `(segmentId) => GeoJSON Feature | undefined` — passed as `id => getSegGeomCache().get(id)` from `Map.jsx`. It is used to attach segment geometry and attributes (FRC, FOW, direction, length) to candidate features. Route geometry (`replay-route`) is built separately in `Map.jsx` using the same two-step geometry fallback.

**`verdictType(verdict)`** maps a `GateVerdict` object to a `ctype` string (`'accepted'`, `'bearing'`, `'radius'`, `'score'`, `'direction'`). It is exported so `TracePanel.jsx` can compute `ctype` values without duplicating the mapping.

### Candidate GeoJSON features

Candidates are **LineString** features (not Points). For each candidate in visual state:
1. Look up segment geometry via `geomLookup(c.segmentId)`
2. If `c.traversal === 'Backward'`, reverse the coordinate array so direction arrows point correctly
3. Merge geometry + segment attributes into the feature properties

All candidate detail is flattened into GeoJSON feature properties:

| Property | Accepted | Rejected |
|---|---|---|
| `ctype` | `'accepted'` | `'bearing'` / `'radius'` / `'score'` / `'direction'` / `'other'` |
| `winner` | true if chosen as leg endpoint | false |
| `segment_id`, `traversal` | ✓ | ✓ (carried at Summary trace level) |
| `snap_lon`, `snap_lat` | ✓ | ✓ |
| `distance_m`, `arc_offset_m`, `bearing_deg` | ✓ | bearing/distance if available |
| `score_total`, `score_distance`, `score_bearing`, `score_frc`, `score_fow`, `score_wrong_ep`, `score_interior` | ✓ | — |
| `verdict_json` | — | JSON-stringified `GateVerdict` |
| `frc`, `fow`, `frc_name`, `fow_name`, `direction`, `length_m`, `source_id` | from geom cache | from geom cache |

### `ReplayPanel.jsx`

Bottom-anchored panel containing:
- **◀ / ▶ step buttons** (also ← / → arrow keys)
- **Step counter**: `N / total · X A* nodes` (A\* badge hidden if 0, which means Summary trace level)
- **Status line**: human-readable description of the current step
- **Full-width trace hint bar**: shown when `totalNodes === 0`, prompting the user to set Trace level → Full
- **Scrubable timeline**: click/drag to jump to any step; colour strips from `replayStats.phases` mark LRP and leg phases

### Map auto-pan behaviour during replay

| Step type | Action |
|---|---|
| `search_started` | `map.flyTo()` to LRP at zoom ≥ 15 |
| `route_search_started` | `map.fitBounds()` between from/to leg endpoints |
| `astar_batch` | `map.jumpTo()` to latest node at zoom 17 (instant, stays in sync with 30 ms playback) |
| `route_found` | `map.fitBounds()` to the full route extent; gold route line pulses for 3 s |

### Candidate click popup

Clicking any candidate dot opens a draggable popup (`cand-panel` CSS class) showing:
- **Accepted**: projection (distance from LRP, arc offset, bearing) + full 7-component score breakdown
- **Rejected**: human-readable gate failure reason (e.g. "Bearing gate — exceeded by 143.97°") + available projection fields
- **Chosen** candidates (winners) show a gold ★ badge in the header

### A\* node colour ramp

`nodeColorAt(t)` maps `t = g_m / maxG ∈ [0, 1]` to a CSS hex colour:
- `t < 0.33`: blue → cyan
- `0.33 ≤ t < 0.66`: cyan → yellow
- `t ≥ 0.66`: yellow → red

### Incremental state update (performance)

`Map.jsx` keeps `replayVisualRef` (last visual state) and `replayStepRef` (its step index). On each render:
- **Forward step**: calls `applyStep` only for new steps — O(1) per step
- **Backward / scrub**: calls `computeVisualState` from step 0 — O(N), acceptable for scrubbing

This avoids the O(N²) cost of recomputing from step 0 on every step during forward playback with thousands of A\* nodes.

---

## 14. File map

| File | Contents |
|---|---|
| `src/main.jsx` | React root mount; `<StrictMode>` wrapper |
| `src/App.jsx` | Startup, WASM init, tile base URL, component tree |
| `src/App.css` | All styles (TopBar, panels, map overlays, trace panel, replay panel, LLM chat) |
| `src/store.js` | Zustand store; 3 module-level caches; `runDecode()`; PRESETS; replay + LLM + encode state (§20) |
| `src/replayEngine.js` | `buildReplaySteps`, `applyStep`, `emptyState`, `computeVisualState`, `stateToGeoJSON(state, geomLookup)`, `verdictType` |
| `src/tileDecoder.js` | OLRL v2 binary → GeoJSON |
| `src/wasm.js` | WASM module loader (`initWasm()`), exposes both the `Decoder` and `Encoder` classes |
| `src/hooks.js` | `useDraggable` |
| `src/diagnosis.js` | `diagnoseFailure`, `diagnoseSuccess` — rule-based decode diagnosis from trace events |
| `src/utils.js` | Small shared helpers |
| `src/llmClient.js` | `chatComplete(config, messages)` — OpenAI-compatible HTTP client |
| `src/llmDiagnosis.js` | `buildSystemContext`/`buildDiagnosticPrompt` (decode) and `buildEncodeDiagnosticPrompt` (encode) — system prompt assembly from current state |
| `src/renderLlmText.jsx` | Lightweight LLM markdown renderer (bold, code, lists) |
| `src/llm/SYSTEM_PROMPT.md` | System prompt template for LLM chat sessions (imported via Vite `?raw`, no build step — see `src/llm/README.md`) |
| `src/llm/tools.js` | LLM function-calling tool schemas + handlers, decode and encode side |
| `src/llm/README.md` | LLM integration documentation |
| `src/components/Map.jsx` | MapLibre GL JS; all sources/layers for both decode and encode mode; tile loader; highlight + replay + candidatePopup effects; waypoint snap-picker popup (§20) |
| `src/components/MenuBar.jsx` | Decode/encode mode toggle; location-type dropdown (encode); panel visibility toggles |
| `src/components/TopBar.jsx` | Input bar, gear menu, Trace level, ▶ Replay button, Decode button |
| `src/components/ParamsPanel.jsx` | DecodeParams editor; FRC/FOW penalty tables; `SpinInput` with optional max |
| `src/components/ResultPanel.jsx` | Decoded segment list; click-to-highlight; failure diagnosis; LLM chat button |
| `src/components/EncodeResultPanel.jsx` | v3/TPEG output + copy buttons; verify-decode glance badge (§20) |
| `src/components/TracePanel.jsx` | Full trace; `SegBtn`; `buildCandPopup`; `RejectedTable`; candidate evaluation popup; forced-decode pin/re-run UI; reads `decodeResult` or `verifyResult` depending on `mode` |
| `src/components/ReplayPanel.jsx` | Step replay UI: ◀/▶ buttons, step counter, scrubable timeline |
| `src/components/BottomBar.jsx` | Status line |
| `src/components/DecodeToast.jsx` | Transient result/error banner |
| `src/components/LlmChatPanel.jsx` | AI chat panel (draggable modal) |
| `src/components/LlmSettingsPanel.jsx` | LLM provider/key/model config (draggable modal) |
| `vite.config.js` | Dev server; serve-tiles plugin (HTTP 206 / range support) |

---

## 15. Candidate colour key

Candidates render as **coloured LineStrings** (`replay-candidates-line`) covering the full segment geometry, with direction triangles (`replay-candidates-arrow`, white SDF icons, 18px spacing) pointing in the traversal direction. Backward candidates have their coordinate arrays reversed before being set as GeoJSON features so the triangles always point in the direction of traversal, not the stored segment direction.

| Line colour | `ctype` value | Meaning |
|---|---|---|
| Bright green (#00ff88), 4px | `accepted`, `winner: true` | Accepted and chosen as leg endpoint |
| Green (#22cc66), 2.5px | `accepted`, `winner: false` | Accepted but beaten by another candidate's route cost |
| Red (#dd3333), 1.5px | `bearing` / `radius` / `score` / `direction` / `other` | Rejected; reason available in the candidate popup |

When the same bidirectional segment is a candidate in both Forward and Backward directions, both appear as separate features with reversed coordinates, producing opposite-direction arrows. To prevent confusion when a specific candidate is selected from the TracePanel, the `replay-candidates-arrow` layer is filtered to only the selected `segment_id` + `traversal` pair while the candidate popup is open.

---

## 16. Direction triangles

All path and candidate line layers use **SDF (Signed Distance Field) direction triangles** (`direction-triangle` image, registered with `{ sdf: true }`). SDF registration allows runtime colour tinting via `icon-color` and halo via `icon-halo-color` / `icon-halo-width` without separate image assets.

| Layer | Source | `icon-color` | `icon-halo-color` | `icon-halo-width` | Spacing |
|---|---|---|---|---|---|
| `decoded-path-arrow` | `decoded-path` | `#004466` (dark navy) | white | 2px | 18px |
| `replay-candidates-arrow` | `replay-candidates` | white | — | — | 18px |
| `trace-segment-arrow` | `trace-segment` | green or red (set via `setPaintProperty`) | white | 4px | 18px |

`trace-segment-arrow` is hidden by default (`visibility: 'none'`) and made visible by the `candidatePopup` effect. Its colour and halo are set via `map.setPaintProperty` at the same time: green (`#22cc66`) for accepted candidates, red (`#ee4444`) for rejected. The larger halo (4px vs 2px) and larger icon size (1.4×) ensure candidate arrows visually dominate over the underlying decoded-path arrows when both occupy the same road.

---

## 17. LLM Chat integration

The **AI Chat** panel provides a conversational interface for decode diagnosis. It is implemented across:

| File | Role |
|---|---|
| `src/llmClient.js` | HTTP client for OpenAI-compatible APIs (OpenAI, Anthropic, Mistral, local); `chatComplete(config, messages)` |
| `src/llmDiagnosis.js` | `buildSystemContext(decodeResult, params)` — constructs a structured system prompt from the current decode result |
| `src/llm/SYSTEM_PROMPT.md` | Template for the system prompt injected into every chat session |
| `src/renderLlmText.jsx` | Renders LLM markdown responses (bold, code, lists) without pulling in a full markdown library |

### Chat lifecycle

1. User clicks **✦ AI Chat** in `ResultPanel`. If no `llmConfig` is set, opens the settings panel instead.
2. `LlmChat.jsx` renders the message history from `llmMessages` store state.
3. On send, `buildSystemContext(decodeResult, params)` is called to produce a system message containing: format, LRP table, candidate summary, route events, failure reason (if any), and current parameter values.
4. `chatComplete(config, messages)` is called; `llmLoading` is set true. On completion, the assistant message is appended to `llmMessages`.
5. The chat session persists across panel open/close within the same decode. `llmMessages` is cleared on a new decode.

### Provider configuration

Stored in `llmConfig` (persisted to localStorage via Zustand `persist`):

```js
{ provider: 'openai' | 'anthropic' | 'mistral' | 'custom', apiKey: string, model: string, baseUrl?: string }
```

---

## 18. Segment diagnosis ("Why didn't the location cover this segment?")

Available from the **ResultPanel** and from segment clicks in the **TracePanel routing section**. Implemented in `src/diagnosis.js`:

- **`diagnoseFailure(decodeResult)`** — called when `decodeResult.ok === false`. Returns `{ headline, bullets, suggestions }` for the failure popup.
- **`diagnoseSuccess(decodeResult)`** — called when `decodeResult.ok === true`. Returns a warning object when the success warrants a caution (e.g., only one candidate per LRP, very short path, large DNP gap).

Both functions read from the trace events embedded in `decodeResult.trace` to produce human-readable summaries without requiring a separate API call.

---

## 19. Known issues / next steps

- **No offline fallback for missing tiles**: if a PMTiles archive is unreachable,
  the decode fails with a generic error. A clear "tile not found" message would help.

- **Segment layer flickers on pan**: `rebuildSource()` replaces the entire GeoJSON
  feature collection on every move. For large tile caches this causes a noticeable
  repaint. A diff-and-patch approach or switching to a tile-protocol source would fix it.

- **TracePanel A\* table capped at 200 rows**: full A\* data is available in the JSON
  via "Copy JSON" but not browseable in the UI for large expansions.

- **Popup position for trace highlights**: when a trace highlight hits multiple segments
  the popup is suppressed. For multi-segment path highlights a summary popup (total
  length, FRC range) would be useful.

- **Rejected candidate `SegBtn` clicks**: clicking a rejected candidate's segment ID
  in `RejectedTable` highlights the segment on the map. However, rejected candidates
  may be outside the loaded tile region (the tile wasn't fetched because the segment
  was beyond the search radius). In that case the highlight silently no-ops.

- **Replay route geometry fallback**: the `replay-route` source uses the same two-step
  segment geometry cache as the trace highlight. If a route segment ID is not in either
  cache (e.g. the tile was not loaded), that segment is silently omitted from the gold
  route line. The route will still show partially in most cases.

---

## 20. Encode mode

Draw waypoints on the map for a Line or PointAlongLine location, get a live routed
preview as you go, and encode to both binary v3 and TPEG-OLR — immediately
round-trip-verified by decoding the result straight back through the ordinary
`Decoder`. Toggled via `MenuBar.jsx`'s mode switch (`store.mode`); only **Line** and
**PointAlongLine** are implemented (§20.1) — the other 7 OpenLR location types are
listed in the location-type dropdown but disabled.

### 20.1 Waypoint editing gesture (`Map.jsx`)

Encode mode dedicates the **right mouse button** entirely to waypoint editing — left
click/drag stays ordinary map panning. (`dragRotate` is disabled and the native
context menu suppressed specifically to free the right button up for this; see the
comment block above `onEncodeMouseDown`.) `mousedown` hit-tests, in priority order:

1. **An existing waypoint marker** — starts a *move* drag (the marker's own listener
   calls `startEncodeDragRef.current('move', index, e)`).
2. **The live route line** (`encode-route-line`/`encode-route-casing`, small pixel
   buffer) — starts an *insert* drag, splitting that leg at a new via-point.
3. **Empty map** — starts an *add* drag (append; for PointAlongLine, replaces the
   single existing point instead, since there's only ever one).

Dragging is optional — a right-click with zero movement and a right-click-drag both
end the same way: `mouseup` always opens the snap-candidate popup at the release
point (§20.3), never commits directly. While dragging, a ghost line
(`updateEncodeGhost`, source `encode-ghost`) redraws live from the fixed neighbor
waypoint(s) to the cursor; `Escape` cancels (`onEncodeDragKeyDown`) without opening
the popup. This gesture model is a deliberate design choice, not an accident: pairing
"where do I click" with "which edit happens" would make append/insert/move
ambiguous near the route line, so priority-ordered hit-testing plus a single
button resolves it structurally instead of guessing intent.

### 20.2 Store fields and actions (`store.js`)

| Field | Type | Description |
|---|---|---|
| `mode` | 'decode' \| 'encode' | Active top-level mode |
| `locationType` | 'Line' \| 'PointAlongLine' \| … | Encode location type (only these two work) |
| `palOrientation`, `palSideOfRoad` | string | PointAlongLine-only encode options; read/write from both the map popup and `EncodeResultPanel` |
| `maxEncodeLegM` | number | Rule-1 cap override, meters; encoder-only, clamped server-side to 15 km |
| `waypoints` | `{lon,lat}[]` | Ordered, user-drawn |
| `waypointHistory` | `waypoints[][]` | Undo stack — prior snapshots pushed before each edit |
| `liveRoute` | `{segments, geometry, length_m}` \| null | From `Encoder.route_between()` |
| `liveRouteError`, `liveRouteLoading` | | Live-preview routing state |
| `encoding` | bool | Encode in flight |
| `encodeResult` | `{v3, tpeg, error}` \| null | Last encode output |
| `verifyResult` | object \| null | The freshly-encoded reference decoded straight back through the same `_decoder` — same shape as `decodeResult`, so Results/Trace/Replay read this instead when `mode === 'encode'` |
| `verifyToast`, `verifyReplaySteps`, `verifyReplayStats`, `verifyReplayStep` | | Encode-side mirrors of the decode replay/toast state |
| `showResult` | bool | Results-panel-family visibility, shared across both modes |

Key actions: `setMode`, `setLocationType`, `addWaypoint`/`insertWaypoint`/
`moveWaypoint`/`removeWaypoint`/`moveWaypointIndex` (each pushes onto
`waypointHistory` first, then calls `runLiveRoute`), `undo`, `clearWaypoints`
(also resets `showResult` to `false` — Clear closes the Results panel along with
wiping the map), `runLiveRoute` (debounced `route_between` call, `needs_tile`
handled the same way `runDecode` handles it), `runEncode`/`runEncodePal` (calls
`encode_line`/`encode_pal`, then `runVerifyDecode` on the result — fire-and-forget,
not awaited by callers, since the panel reads the in-flight state reactively),
`runVerifyDecode`, `openResult`/`hideResult`/`toggleResult`.

`previewRouteBetween(waypointsList, maxTurnDeviationDeg)` is a separate, exported,
**stateless** function mirroring `runLiveRoute`'s core call but touching no store
state — used so clicking through multiple snap candidates in the popup (§20.3) to
compare routes doesn't pollute `waypointHistory` the way repeated real
`insertWaypoint`/`moveWaypoint` calls would.

### 20.3 Waypoint snap-candidate popup

Opened by `showWaypointPopup(lon, lat, kind, index)` on every `mouseup` from §20.1
(`kind` is `'add' | 'insert' | 'move'`). Queries nearby segments/nodes and lists them
as clickable candidate rows in a MapLibre `Popup`:

- **Click-only selection** — hovering a row does nothing; only a click updates the
  active candidate, its highlight, and (via `previewRouteBetween`) the live route
  preview. This is deliberate: a cheap hover-preview was tried and rejected because
  it changed the selection without the user asking it to.
- **Distinct click-point marker** — the actual coordinate the user right-clicked is
  rendered on a separate source/layer (`encode-click-point`, bright pink-red circle)
  from the candidate snap points (blue/grey), so the two are never visually confused.
- **Draggable popup** — the popup title doubles as a drag handle
  (`.snap-picker-drag-handle`); dragging calls `popup.setOffset([x, y])` (tracked in a
  local `currentOffset`, not read back from the popup — `Popup` has no `getOffset()`,
  only `Marker` does) so the user can move the popup out of the way of the geometry
  it's showing, rather than the app trying to out-guess placement heuristically. An
  anchor is still computed on open (averaging the direction to neighbor waypoints and
  candidates, with a viewport-fit check before forcing it) as a reasonable starting
  position, but dragging is the actual fix for occlusion, not the anchor heuristic.
- **"Last waypoint" checkbox** (Line only) — disabled for a location's first waypoint
  (nothing to draw yet). Checking it and pressing Enter (or clicking it directly)
  calls `commitCandidate`, which commits the point *and* — if this was the last Line
  waypoint, or always for PointAlongLine (implicitly "last") — immediately opens the
  Results panel and fires `runEncode`/`runEncodePal`, so confirming the final point is
  the one action that finishes the whole flow.

### 20.4 `EncodeResultPanel.jsx`

Shows the last `encodeResult` (v3 base64 / TPEG hex, each with a copy button) and a
compact glance badge derived from `verifyResult` (✓ round-tripped cleanly / ⚠ verify
mismatch or failure). Full drill-down uses the **same** `ResultPanel`/`TracePanel`/
`ReplayPanel` components decode mode uses — they simply read `verifyResult` instead
of `decodeResult` while `mode === 'encode'` (see §20.2), so there is no separate
encode-side trace UI to maintain.

### 20.5 WASM `Encoder` bindings (`crates/openlr-wasm/src/lib.rs`)

A separate class from `Decoder`, mirroring its tile lifecycle (`new`, `load_tile`,
`reset_tiles`, `tiles_near_point`): `route_between(waypoints_json, max_turn_deviation_deg, zoom)`
(Layer-1 preview routing — snap + chain A* between waypoints, `needs_tile` retry
contract like `Decoder::decode`), `candidates_near_point(lon, lat)` (feeds §20.3's
popup), `encode_line(...)`/`encode_pal(...)` (calls into `openlr-encoder`, then
`openlr-codec`'s v3/TPEG serializers), plus the diagnostic bindings
`diagnose_connection`/`check_boundary_expansion`/`get_turn_deviation` also exposed as
LLM chat tools (`src/llm/tools.js`) — see `CLAUDE.md` §11 for what each does
internally and §10/Invariant 10 for why `check_boundary_expansion` takes an
`end_side` parameter.

### 20.6 A bug class worth knowing before touching snapping/expansion code

Three separate bugs this feature shipped with (two in `snap_point`, one in Rule-4
boundary expansion) were the same root cause: using `Graph::topology_neighbors()`
(direction-agnostic — real-world topology, ignoring one-way restrictions) to pick an
anchor/travel-direction somewhere with no subsequent A*/routing step to catch a
wrong-direction pick. `Graph::outgoing_segments()` is the direction-aware
alternative. See `CLAUDE.md` Invariant 10 before adding new anchor-selection logic
anywhere in the encode path.

## 21. Deployment (Cloudflare Pages + R2)

Production hosting is Cloudflare Pages for the static SPA, with the (multi-GB)
`world.pmtiles` archive in an R2 bucket instead of local disk — R2 has no egress
fees, which matters given how many range-read requests a single session makes
against a large archive. A Pages Function proxies tile requests to R2 rather than
exposing the bucket directly, so it's same-origin with the SPA (no CORS needed) and
shares the Pages project's deploy lifecycle.

**Pieces:**
- `web/functions/tiles/[[path]].js` — catch-all route for `GET /tiles/*`. Reads the
  object from the `TILES_BUCKET` R2 binding, forwarding the browser's `Range` header
  and returning a real `206`/`Content-Range` when `object.range` comes back populated
  (not just a flat `200` with an ignored range — pmtiles-js and MapLibre both depend
  on genuine partial-content responses to avoid pulling the whole multi-GB archive).
- `web/wrangler.toml` — declares the Pages project name, `pages_build_output_dir =
  "dist"`, and the `TILES_BUCKET` R2 binding (bucket name `openlr-lab-tiles`).
- `web/.env.production` — sets `VITE_TILE_BASE_URL=/tiles`, so a production build's
  default tile base is the same-origin Pages Function instead of the local dev
  server. Local dev is unaffected (falls back to `http://localhost:5176`, matching
  `vite.config.js`'s dev-only tile server — see §1). This env var is read in four
  places that used to hardcode the localhost default: `store.js`'s `tileUrl` initial
  state, `App.jsx`'s `resolveBase()`, `MenuBar.jsx`'s tile-menu sync effect, and
  `TopBar.jsx`'s `effectiveUrl` derivation.

**One-time setup:**
1. `npx wrangler login` (OAuth against the Cloudflare account).
2. Enable R2 once via the dashboard (R2 Object Storage → enable) — not scriptable,
   the API rejects bucket creation until this has been clicked through once per
   account.
3. `npx wrangler r2 bucket create openlr-lab-tiles`
4. `npx wrangler pages project create openlr-lab --production-branch=main`

**Getting tiles into R2** (manual, tied to whenever `openlr-pmtiles`'s `build-world`
finishes a fresh archive — not automated, a completely separate tool/schedule):
```
wrangler r2 object put openlr-lab-tiles/world.pmtiles --file=/path/to/world.pmtiles
wrangler r2 object put openlr-lab-tiles/manifest.json --file=/path/to/manifest.json
```

**Day-to-day deploy** (no CI — built and deployed locally):
```
cd web && npm run deploy
```
which runs `wasm-pack build` (crates/openlr-wasm → web/src/wasm, gitignored, must be
fresh locally before every deploy since it's never committed), `vite build`, then
`wrangler pages deploy dist --project-name=openlr-lab`.
