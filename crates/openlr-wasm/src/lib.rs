//! WebAssembly bindings for the OpenLRLens decode engine.
//!
//! # JS usage pattern
//!
//! ```js
//! import init, { Decoder } from './openlr_wasm.js';
//! await init();
//!
//! const dec = new Decoder();
//!
//! // 1. Parse the reference and learn which tiles are needed.
//! const { tiles } = JSON.parse(dec.start("CwRbnh...", JSON.stringify(params), 12));
//! // tiles: [[z, x, y], ...]
//!
//! // 2. Fetch each tile from the PMTiles archive and inject it.
//! for (const [z, x, y] of tiles) {
//!     const bytes = await pmtilesSource.getZxy(z, x, y);
//!     if (bytes) dec.load_tile(z, x, y, new Uint8Array(bytes));
//! }
//!
//! // 3. Run the decode.
//! const result = JSON.parse(dec.decode());
//! if (result.ok) {
//!     console.log(result.wkt);          // "LINESTRING (...)"
//!     console.log(result.segments);     // [{ frc, fow, osm_way_id }, ...]
//! }
//! ```

use wasm_bindgen::prelude::*;

use openlr_codec::{decode_v3_base64, decode_tpeg_hex, decode_tpeg_base64};
use openlr_codec::lrp::LocationReference;
use openlr_engine::{decode as engine_decode, DecodeParams, Preset, prefetch_tile_keys, path_to_wkt, path_band_wkt};
use openlr_graph::polyline_length_m;
use openlr_engine::trace::TraversalDir;
use openlr_provider::TileLoader;
use serde::Serialize;

// ── Module init ───────────────────────────────────────────────────────────────

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

// ── JS-visible result types ───────────────────────────────────────────────────

/// Returned by `Decoder.start()` as a JSON string.
#[derive(Serialize)]
struct StartResult {
    /// Tiles to fetch before calling `decode()`.  Each entry is `[z, x, y]`.
    tiles: Vec<[u32; 3]>,
}

/// Per-segment metadata included in a successful `DecodeResult`.
#[derive(Serialize)]
struct SegmentInfo {
    frc: u8,
    fow: u8,
    /// OSM way ID, present when the tile was built from OSM data.
    #[serde(skip_serializing_if = "Option::is_none")]
    osm_way_id: Option<i64>,
    /// Source tile key, e.g. `"12/2135/1425"`.  Used by the UI to highlight the segment.
    tile: String,
    /// Segment's index within its source tile (matches the GeoJSON `local_index` property).
    local_index: u32,
    /// Internal graph segment ID assigned during tile loading.  Matches the `segment_id`
    /// values in the decode trace (candidate rankings, routing events).
    segment_id: u32,
    /// Geometry as `[[lon, lat], ...]` — used by the UI to draw a dedicated highlight layer.
    geometry: Vec<[f64; 2]>,
}

/// Per-LRP metadata included in every `DecodeResult` (success or failure).
#[derive(Serialize)]
struct LrpInfo {
    lon: f64,
    lat: f64,
    frc: u8,
    fow: u8,
    /// Absent on the last LRP.
    #[serde(skip_serializing_if = "Option::is_none")]
    lfrcnp: Option<u8>,
    bearing_lb: f64,
    bearing_ub: f64,
}

/// Returned by `Decoder.decode()` as a JSON string.
#[derive(Serialize)]
struct DecodeResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    wkt: Option<String>,
    segments: Vec<SegmentInfo>,
    lrps: Vec<LrpInfo>,
    /// Midpoint of the positive-offset interval (meters from first LRP forward).
    pos_offset_m: f64,
    /// Midpoint of the negative-offset interval (meters backward from last LRP).
    neg_offset_m: f64,
    /// Raw [LB, UB] of the positive offset interval. Both 0 when no pos offset.
    pos_offset_lb: f64,
    pos_offset_ub: f64,
    /// Raw [LB, UB] of the negative offset interval. Both 0 when no neg offset.
    neg_offset_lb: f64,
    neg_offset_ub: f64,
    /// WKT of the v3 uncertainty band at the path head (pos offset LB→UB).
    /// Absent when offset is zero or point-exact (TPEG).
    #[serde(skip_serializing_if = "Option::is_none")]
    pos_uncertainty_wkt: Option<String>,
    /// WKT of the v3 uncertainty band at the path tail (neg offset LB→UB).
    #[serde(skip_serializing_if = "Option::is_none")]
    neg_uncertainty_wkt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Full decode trace; null when `trace_level` is `Off` or on error.
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<serde_json::Value>,
}

impl DecodeResult {
    fn err(msg: impl Into<String>) -> Self {
        DecodeResult {
            ok: false,
            wkt: None,
            segments: vec![],
            lrps: vec![],
            pos_offset_m: 0.0,
            neg_offset_m: 0.0,
            pos_offset_lb: 0.0,
            pos_offset_ub: 0.0,
            neg_offset_lb: 0.0,
            neg_offset_ub: 0.0,
            pos_uncertainty_wkt: None,
            neg_uncertainty_wkt: None,
            error: Some(msg.into()),
            trace: None,
        }
    }
}

// ── Decoder ───────────────────────────────────────────────────────────────────

/// Stateful decode session.  Create one per reference string, or call `reset()`
/// between decodes if you want to reuse the loaded tile cache.
#[wasm_bindgen]
pub struct Decoder {
    loader: TileLoader,
    location_ref: Option<LocationReference>,
    params: DecodeParams,
    zoom: u8,
}

#[wasm_bindgen]
impl Decoder {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Decoder {
        Decoder {
            loader: TileLoader::new(),
            location_ref: None,
            params: DecodeParams::default(),
            zoom: 12,
        }
    }

    /// Parse `openlr_string` (auto-detects OpenLR binary v3 base64 or TPEG-OLR hex),
    /// store the decode parameters, and compute the set of tiles that must be loaded.
    ///
    /// `params_json`: JSON-serialized `DecodeParams`, or `""` / `"null"` for defaults.
    /// `zoom`: tile zoom level (must match the PMTiles archive; typically 12).
    ///
    /// Returns a JSON string: `{ "tiles": [[z, x, y], ...] }`.
    /// Throws a JS error string on parse failure.
    pub fn start(&mut self, openlr_string: &str, params_json: &str, zoom: u8) -> Result<String, JsValue> {
        let params: DecodeParams = match params_json {
            "" | "null" | "Default" => DecodeParams::default(),
            "Permissive" => DecodeParams::preset(Preset::Permissive),
            "Strict"     => DecodeParams::preset(Preset::Strict),
            other => serde_json::from_str(other)
                .map_err(|e| JsValue::from_str(&format!("invalid params: {e}")))?,
        };

        let loc_ref = parse_openlr(openlr_string)
            .map_err(|e| JsValue::from_str(&e))?;

        let tile_keys = prefetch_tile_keys(&loc_ref.lrps, &params, zoom);
        let tiles: Vec<[u32; 3]> = tile_keys
            .iter()
            .map(|k| [k.z as u32, k.x, k.y])
            .collect();

        self.location_ref = Some(loc_ref);
        self.params = params;
        self.zoom = zoom;

        Ok(serde_json::to_string(&StartResult { tiles }).unwrap())
    }

    /// Inject one tile's raw OLRL bytes into the graph.  Call once per tile
    /// returned by `start()`.  Missing tiles are silently skipped — decode will
    /// simply have fewer candidates near those coordinates.
    ///
    /// Throws a JS error string if the tile payload is malformed.
    pub fn load_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<(), JsValue> {
        self.loader
            .load_tile_at(z, x, y, data)
            .map_err(|e| JsValue::from_str(&format!("tile parse error: {e}")))
    }

    /// Run the decode against the loaded graph.
    ///
    /// Returns a JSON string.  On success:
    /// ```json
    /// { "ok": true, "wkt": "LINESTRING (...)", "segments": [...],
    ///   "pos_offset_m": 0.0, "neg_offset_m": 0.0, "trace": {...} }
    /// ```
    /// On failure:
    /// ```json
    /// { "ok": false, "error": "LRP 0: no candidate segments found", "segments": [] }
    /// ```
    pub fn decode(&self) -> String {
        let loc_ref = match &self.location_ref {
            Some(r) => r,
            None => return serde_json::to_string(&DecodeResult::err("call start() first")).unwrap(),
        };

        let lrps: Vec<LrpInfo> = loc_ref.lrps.iter().map(|lrp| LrpInfo {
            lon: lrp.coord.0,
            lat: lrp.coord.1,
            frc: lrp.frc,
            fow: lrp.fow,
            lfrcnp: lrp.lfrcnp,
            bearing_lb: lrp.bearing.lb_deg,
            bearing_ub: lrp.bearing.ub_deg,
        }).collect();

        let result = match engine_decode(loc_ref, &self.loader.graph, &self.params) {
            Err(e) => return serde_json::to_string(&DecodeResult {
                lrps,
                ..DecodeResult::err(e.to_string())
            }).unwrap(),
            Ok(r) => r,
        };

        let wkt = path_to_wkt(
            &result.path,
            result.pos_offset_m,
            result.neg_offset_m,
            result.first_lrp_arc_m,
            result.last_lrp_arc_m,
            result.first_seg_traversal,
            result.last_seg_traversal,
            &self.loader.graph,
        );

        let n_path = result.path.len();
        let segments: Vec<SegmentInfo> = result.path.iter().enumerate().filter_map(|(i, seg_id)| {
            self.loader.graph.segments.get(seg_id).map(|seg| {
                let (tile, local_index) = self.loader.seg_tile.get(seg_id)
                    .map(|&(z, x, y, li)| (format!("{z}/{x}/{y}"), li))
                    .unwrap_or_else(|| ("unknown".to_string(), 0));
                // Use the explicit traversal direction for the first and last segments so
                // the UI highlight geometry runs in the correct direction.  Interior
                // segments are stored in their natural connectivity order and need no flip.
                let traversal = if i == 0 {
                    result.first_seg_traversal
                } else if i == n_path - 1 {
                    result.last_seg_traversal
                } else {
                    TraversalDir::Forward
                };
                let geometry: Vec<[f64; 2]> = match traversal {
                    TraversalDir::Forward  => seg.geometry.iter().map(|&(lon, lat)| [lon, lat]).collect(),
                    TraversalDir::Backward => seg.geometry.iter().rev().map(|&(lon, lat)| [lon, lat]).collect(),
                };
                SegmentInfo {
                    frc: seg.frc,
                    fow: seg.fow,
                    osm_way_id: seg.osm_way_id(),
                    tile,
                    local_index,
                    segment_id: seg_id.0,
                    geometry,
                }
            })
        }).collect();

        // ── Offset intervals and uncertainty bands ──────────────────────────
        // The v3 bucket turns each offset into a [lb, ub] interval; the engine
        // uses the midpoint as the actual trim.  Expose the full interval so the
        // UI can draw the uncertainty band between lb and ub.
        let pos_int = loc_ref.lrps.first().and_then(|l| l.pos_offset);
        let neg_int = loc_ref.lrps.last() .and_then(|l| l.neg_offset);

        let (pos_offset_lb, pos_offset_ub) = pos_int.map(|i| (i.lb, i.ub)).unwrap_or((0.0, 0.0));
        let (neg_offset_lb, neg_offset_ub) = neg_int.map(|i| (i.lb, i.ub)).unwrap_or((0.0, 0.0));

        // Compute per-segment arc lengths once so both uncertainty bands can reuse them.
        let actual_lens: Vec<f64> = result.path.iter()
            .filter_map(|id| self.loader.graph.segments.get(id))
            .map(|s| polyline_length_m(&s.geometry))
            .collect();
        let last_seg_len = actual_lens.last().copied().unwrap_or(0.0);

        // Positive uncertainty: band from (first_lrp_arc + lb) to (first_lrp_arc + ub).
        // Only meaningful when lb < ub (i.e., v3 encoding; TPEG has lb == ub).
        let pos_uncertainty_wkt = pos_int
            .filter(|i| i.ub > i.lb)
            .and_then(|i| path_band_wkt(
                &result.path,
                result.first_lrp_arc_m + i.lb,
                result.first_lrp_arc_m + i.ub,
                result.first_seg_traversal,
                &self.loader.graph,
            ));

        // Negative uncertainty: band at the tail.
        // Last LRP position from path start = sum(segs[0..n-2]) + last_lrp_arc_m.
        let last_lrp_pos_from_start: f64 = actual_lens[..actual_lens.len().saturating_sub(1)]
            .iter().sum::<f64>() + result.last_lrp_arc_m.min(last_seg_len);
        let neg_uncertainty_wkt = neg_int
            .filter(|i| i.ub > i.lb)
            .and_then(|i| path_band_wkt(
                &result.path,
                (last_lrp_pos_from_start - i.ub).max(0.0),
                last_lrp_pos_from_start - i.lb,
                result.first_seg_traversal,
                &self.loader.graph,
            ));

        let trace_value = result.trace.and_then(|t| serde_json::to_value(t).ok());

        serde_json::to_string(&DecodeResult {
            ok: true,
            wkt,
            segments,
            lrps,
            pos_offset_m: result.pos_offset_m,
            neg_offset_m: result.neg_offset_m,
            pos_offset_lb,
            pos_offset_ub,
            neg_offset_lb,
            neg_offset_ub,
            pos_uncertainty_wkt,
            neg_uncertainty_wkt,
            error: None,
            trace: trace_value,
        }).unwrap()
    }

    /// Clear the stored location reference.  The loaded tile graph is kept so
    /// nearby re-decodes can reuse the cached tiles — call `reset_tiles()` too
    /// if you want to start completely fresh.
    pub fn reset(&mut self) {
        self.location_ref = None;
    }

    /// Drop all loaded tiles and the stored location reference.
    pub fn reset_tiles(&mut self) {
        self.loader = TileLoader::new();
        self.location_ref = None;
    }

    /// Tile zoom level in use (set by `start()`).
    pub fn zoom(&self) -> u8 {
        self.zoom
    }

    /// Return the internal graph segment ID for the segment at `(z, x, y, local_index)`,
    /// or -1 if that tile/index combination is not currently loaded.
    /// Useful for correlating map-click segments with trace log `segment_id` values.
    ///
    /// Returns `f64` rather than `i64` so JS receives a plain Number (not BigInt).
    /// All segment IDs are u32-bounded, so no precision is lost.
    pub fn segment_id_at(&self, z: u8, x: u32, y: u32, local_index: u32) -> f64 {
        self.loader.seg_tile.iter()
            .find(|(_, &(sz, sx, sy, sl))| sz == z && sx == x && sy == y && sl == local_index)
            .map(|(id, _)| id.0 as f64)
            .unwrap_or(-1.0)
    }

    /// Return all loaded segment→tile mappings as a JSON string.
    ///
    /// Each entry is `[segment_id, z, x, y, local_index]`.  This is the O(n) alternative
    /// to calling `segment_id_at` in a JS loop (which is O(n²) due to repeated linear scans).
    ///
    /// Used by the JS layer to build its segment_id → tile reverse-lookup map.
    pub fn all_segment_tile_mappings(&self) -> String {
        let mappings: Vec<[u32; 5]> = self.loader.seg_tile.iter()
            .map(|(id, &(z, x, y, li))| [id.0, z as u32, x, y, li])
            .collect();
        serde_json::to_string(&mappings).unwrap()
    }

    /// Return how many segments were loaded from tile `(z, x, y)`, or 0 if not loaded.
    pub fn tile_segment_count(&self, z: u8, x: u32, y: u32) -> u32 {
        self.loader.seg_tile.values()
            .filter(|&&(sz, sx, sy, _)| sz == z && sx == x && sy == y)
            .count() as u32
    }

    /// Number of segments in the loaded graph.
    pub fn loaded_segment_count(&self) -> usize {
        self.loader.graph.segments.len()
    }

    /// Number of nodes in the loaded graph.
    pub fn loaded_node_count(&self) -> usize {
        self.loader.graph.nodes.len()
    }
}

// ── Format auto-detection ─────────────────────────────────────────────────────

/// Try OpenLR binary v3 (base64) then TPEG-OLR (hex).  Returns the first that parses.
fn parse_openlr(s: &str) -> Result<LocationReference, String> {
    // Strategy: try every plausible interpretation in order of specificity.
    // For base64-like input (contains +/=, or has base64-valid length), we try both
    // v3 and TPEG decoders on the decoded bytes.
    // For hex-like input (all hex chars, even length) we try TPEG hex last.
    let has_base64_chars = s.chars().any(|c| c == '+' || c == '/' || c == '=');

    // Try v3 base64 first (most common).
    if has_base64_chars || looks_like_base64(s) {
        if let Ok(r) = decode_v3_base64(s) { return Ok(r); }
        if let Ok(r) = decode_tpeg_base64(s) { return Ok(r); }
    }

    // Try TPEG hex (hex string of even length).
    if let Ok(r) = decode_tpeg_hex(s) { return Ok(r); }

    // Last resort: try v3 base64 anyway (url-safe base64 without padding).
    decode_v3_base64(s)
        .map_err(|e| format!("OpenLR parse error (tried v3 base64, TPEG base64, TPEG hex): {e}"))
}

fn looks_like_base64(s: &str) -> bool {
    // Heuristic: all chars are base64url-safe, and length is 4-byte aligned (with or without padding).
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && s.len() % 4 == 0
}
