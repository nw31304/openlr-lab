# MCP Tool Implementation Spec

This document specifies the Rust/WASM changes needed to expose the MCP tool
contracts defined in `OpenLR_LLM_Context.md`. Changes are additive — no
existing interfaces break.

---

## 1. `crates/openlr-engine/src/trace.rs`

### 1a. Extend `RouteFound`

Add winning attempt index and per-segment breakdown:

```rust
pub struct RouteSegment {
    pub segment_id: SegmentId,
    pub frc: u8,
    pub fow: u8,
    pub length_m: f64,
}

// In DecodeEvent::RouteFound — add:
RouteFound {
    leg: usize,
    path: Vec<SegmentId>,
    length_m: f64,
    from_snap: (f64, f64),
    to_snap: (f64, f64),
    attempt_index: usize,           // NEW: which permutation succeeded
    segments: Vec<RouteSegment>,    // NEW: per-segment FRC/FOW/length
},
```

The engine already has segment metadata when assembling the route path; the
`RouteSegment` list is a parallel traversal of the same path. `attempt_index`
is the loop counter in the `RouteGenerator` iteration.

### 1b. Extend `OffsetApplied`

Add the geographic trim point:

```rust
OffsetApplied {
    is_positive: bool,
    interval: LinearInterval,
    trim_lon: f64,    // NEW
    trim_lat: f64,    // NEW
},
```

The engine computes the trim point when applying offsets; emit it here.

### 1c. New tile lifecycle events

```rust
// Add to DecodeEvent:
TileLoaded {
    z: u8,
    x: u32,
    y: u32,
    segment_count: u32,
},
TileFailed {
    z: u8,
    x: u32,
    y: u32,
    reason: TileFailReason,
},

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TileFailReason {
    NetworkError,
    NotFound,
    ParseError,
}
```

`TileLoaded` is emitted from the WASM layer (`Decoder::load_tile`) after a tile
is successfully parsed and added to the `TileLoader`. `TileFailed` is emitted
if the caller signals a fetch failure (a new `load_tile_failed(z, x, y, reason)`
WASM method — see §3).

---

## 2. `crates/openlr-engine/src/lib.rs`

No structural changes to the `decode()` function signature. The engine returns
`DecodedLocation`; the WASM layer extracts MCP-queryable state from the trace
rather than requiring a parallel return channel.

The one exception: `retry_leg` requires the engine to accept forced
from/to candidates for a single leg. Add a helper:

```rust
/// Run A* for one leg with a forced candidate pair and return the route result.
/// Used by the WASM `retry_leg` tool.
pub fn route_single_leg(
    from: &ScoredCandidate,
    to: &ScoredCandidate,
    lrp_idx: usize,       // for LFRCNP lookup
    reference: &LocationReference,
    provider: &impl OpenLRDataProvider,
    params: &DecodeParams,
) -> Result<(Vec<SegmentId>, f64), RoutingFailure>
```

This is a thin wrapper over the existing `find_route` internals, bypassing the
`RouteGenerator` permutation loop.

---

## 3. `crates/openlr-wasm/src/lib.rs`

### 3a. Retained post-decode state

The `Decoder` struct must hold intermediate results after `decode()` completes
so MCP tools can query them without re-running the decode.

```rust
pub struct Decoder {
    // existing fields ...
    tile_loader: TileLoader,
    parsed_reference: Option<LocationReference>,

    // NEW: retained decode state
    last_trace: Option<DecodeTrace>,
    last_result: Option<DecodedLocation>,
}
```

The existing `DecodeTrace` already contains `CandidatesRanked` events (with
full `ScoredCandidate` / `RejectedCandidate` lists) and `RouteFound` events
(with `segments` after §1a). MCP tools extract their data by filtering
`last_trace.events` by event type and LRP/leg index — no separate storage
needed.

For `get_astar_trace`, Full-level A\* events are not in the Summary trace.
Re-run A\* for the requested leg on demand (see §3d).

### 3b. New WASM-exported methods

All return `JsValue` (serialised TOON string or JSON fallback). All return
`null` if the decode hasn't been run or the index is out of range.

```rust
#[wasm_bindgen]
impl Decoder {

    /// Tier 1 — always call first.
    pub fn get_decode_summary(&self) -> JsValue

    /// Tier 2 — parsed LRP structure.
    pub fn get_parsed_reference(&self) -> JsValue

    /// Tier 3 — candidates for one LRP.
    /// include_rejected: whether to include rejected candidates in result.
    pub fn get_lrp_candidates(&self, lrp_index: usize, include_rejected: bool) -> JsValue

    /// Tier 4a — routing summary for one leg.
    pub fn get_leg_summary(&self, leg_index: usize) -> JsValue

    /// Tier 4b — per-segment breakdown of a successfully found route.
    pub fn get_route_segments(&self, leg_index: usize) -> JsValue

    /// Tier 4c — paginated A* expansion/skip events.
    /// Triggers a Full-level re-run of A* for this leg if not already cached.
    pub fn get_astar_trace(&self, leg_index: usize, offset: usize, limit: usize) -> JsValue

    /// Tier 5a — full attributes + geometry for one segment.
    pub fn get_segment(&self, segment_id: u64) -> JsValue

    /// Tier 5b — segments near a coordinate.
    pub fn get_segments_near(&self, lat: f64, lon: f64, radius_m: f64) -> JsValue

    /// Tier 6a — re-score one segment against one LRP with modified params.
    /// params_override_json: partial DecodeParams JSON (merged over current params).
    pub fn rescore_candidate(
        &self,
        lrp_index: usize,
        segment_id: u64,
        params_override_json: &str,
    ) -> JsValue

    /// Tier 6b — full re-decode with modified params (tiles already loaded).
    pub fn retry_decode(&mut self, params_override_json: &str) -> JsValue

    /// Tier 6c — force a specific candidate pair for one leg and re-run A*.
    pub fn retry_leg(
        &self,
        leg_index: usize,
        from_segment_id: u64,
        to_segment_id: u64,
        params_override_json: &str,
    ) -> JsValue

    /// Signal that a tile fetch failed (emits TileFailed trace event).
    pub fn load_tile_failed(&mut self, z: u8, x: u32, y: u32, reason: &str)
}
```

### 3c. `get_astar_trace` — on-demand Full re-run

Since A\* Full events are not kept in the Summary trace, `get_astar_trace`
triggers a targeted re-run:

1. Extract the winning `from`/`to` candidates for the requested leg from
   `last_trace` (`RouteFound` event for that leg).
2. Call `route_single_leg` (§2) with `trace_level = Full`.
3. Cache the resulting Full trace in a per-leg store (e.g.
   `astar_full_traces: Vec<Option<Vec<DecodeEvent>>>`).
4. Slice the cached event list by `[offset, offset+limit)` and serialise.

Subsequent calls for the same leg return from cache; no re-run.

### 3d. `params_override_json` merging

All recomputation tools accept a partial params JSON that is merged over the
current params. Implement as:

```rust
fn merge_params(base: &DecodeParams, override_json: &str) -> Result<DecodeParams, _> {
    // Deserialise override into serde_json::Value
    // Merge into serialised base, re-deserialise
    // Missing fields inherit from base
}
```

This allows the LLM to pass `{"max_bearing_deviation_deg": 30}` without
specifying every other parameter.

### 3e. TOON serialisation

Add a `toon` feature flag or a `to_toon(value: &serde_json::Value) -> String`
helper that converts the JSON response to TOON before returning. Use the
Rust TOON crate (community implementation). Fall back to JSON if TOON
serialisation fails.

All MCP tool methods serialise their return value through this helper.

---

## 4. MCP server transport (JS layer)

The WASM `Decoder` exposes the tools; the JS layer wraps them as an MCP
HTTP+SSE server. Recommended approach for v1:

```
Browser tab
  └── Vite dev server (or built app)
       └── Service worker intercepts POST /mcp/tools/call
            └── Routes to Decoder.<method>() in the main thread via MessageChannel
            └── Returns TOON response as SSE event
```

Each MCP tool call:
1. JS receives `{ tool: "get_lrp_candidates", arguments: { lrp_index: 1, include_rejected: false } }`
2. JS calls `decoder.get_lrp_candidates(1, false)` → TOON string
3. JS wraps in MCP tool result envelope and returns

The service worker approach requires no extra process and works for both dev
and production deployments. For Claude Desktop integration, a thin local relay
(Node script) can bridge stdio MCP ↔ the app's HTTP endpoint.

---

## 5. Implementation order

1. **Trace additions** (§1) — `RouteSegment`, `attempt_index`, `trim_lon/lat`,
   `TileLoaded`, `TileFailed`. These are purely additive; existing consumers
   are unaffected.
2. **`load_tile_failed`** WASM method + `TileFailed` emission.
3. **Retained state** in `Decoder` (`last_trace`, `last_result`) + wire up in
   existing `decode()`.
4. **Read-only tools** (`get_decode_summary` through `get_segments_near`) —
   these only parse `last_trace` and query `tile_loader`.
5. **`rescore_candidate`** — single-segment scoring, no routing.
6. **`route_single_leg`** engine helper + `retry_leg` WASM method.
7. **`retry_decode`** — re-runs `decode()` with merged params.
8. **`get_astar_trace`** — on-demand Full re-run + cache.
9. **TOON serialisation** — swap in after read-only tools are tested with JSON.
10. **MCP transport** (service worker or local relay).

Steps 1–4 can be implemented and tested independently of the MCP transport.
Steps 5–8 add recomputation. Step 9 is a drop-in swap. Step 10 wires
everything to the LLM.
