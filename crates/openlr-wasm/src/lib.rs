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
//!     console.log(result.segments);     // [{ frc, fow, stable_id }, ...]
//! }
//! ```

use std::collections::HashSet;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn warn(s: &str);
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

use openlr_codec::{decode_v3_base64, decode_tpeg_hex, decode_tpeg_base64};
use openlr_codec::lrp::{LocationReference, Orientation, SideOfRoad};
use openlr_engine::DecodeResult as EngineDecodeResult;
use openlr_engine::{decode as engine_decode, decode_forced as engine_decode_forced, DecodeError, DecodeParams, Preset, prefetch_tile_keys, path_to_wkt, path_band_wkt};
use openlr_engine::{ScoredCandidate, ProjectionResult, CandidateScore};
use openlr_graph::{SegmentId, NodeId};
use openlr_graph::{polyline_length_m, haversine_m, Direction};
use openlr_graph::{Graph, TileKey, PathOutcome, PathResult, shortest_path, project_onto_polyline, NO_PRIOR_SEG};
use openlr_engine::trace::TraversalDir;
use openlr_provider::TileLoader;
use openlr_encoder::line::{encode_line as enc_line, LineLocationInput};
use openlr_encoder::pal::{encode_pal as enc_pal, PalLocationInput};
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

/// Returned by `Decoder.decode()` when A* needs a tile that has not been loaded yet.
/// JS must load the tile via `load_tile()` and call `decode()` again.
#[derive(Serialize)]
struct NeedsTileResult {
    needs_tile: [u32; 3],
}

/// Per-segment metadata included in a successful `DecodeResult`.
#[derive(Serialize)]
struct SegmentInfo {
    frc: u8,
    fow: u8,
    /// Traversal direction: "Both", "Forward", or "Backward".
    direction: &'static str,
    /// Segment length in metres (precomputed; not re-derived from geometry).
    length_m: f64,
    /// Opaque stable identifier supplied by the tile provider.
    stable_id: String,
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
    /// Distance-to-next-point interval in metres. Absent on the last LRP.
    #[serde(skip_serializing_if = "Option::is_none")]
    dnp_lb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dnp_ub: Option<f64>,
    /// Snap point on the matched segment (lon, lat). Absent on decode failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_lat: Option<f64>,
    /// True when snap landed on a segment endpoint node; false for interior projection.
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_is_endpoint: Option<bool>,
    /// Distance from encoded LRP coordinate to snap point, metres.
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_distance_m: Option<f64>,
}

fn lrp_info_vec(
    lrps: &[openlr_codec::lrp::Lrp],
    snap_points: &[(f64, f64)],
    snap_is_endpoint: &[bool],
    snap_distances_m: &[f64],
) -> Vec<LrpInfo> {
    lrps.iter().enumerate().map(|(i, lrp)| LrpInfo {
        lon: lrp.coord.0,
        lat: lrp.coord.1,
        frc: lrp.frc,
        fow: lrp.fow,
        lfrcnp: lrp.lfrcnp,
        bearing_lb: lrp.bearing.lb_deg,
        bearing_ub: lrp.bearing.ub_deg,
        dnp_lb: lrp.dnp.map(|d| d.lb),
        dnp_ub: lrp.dnp.map(|d| d.ub),
        snap_lon: snap_points.get(i).map(|p| p.0),
        snap_lat: snap_points.get(i).map(|p| p.1),
        snap_is_endpoint: snap_is_endpoint.get(i).copied(),
        snap_distance_m: snap_distances_m.get(i).copied(),
    }).collect()
}

/// Returned by `Decoder.decode()` as a JSON string.
#[derive(Serialize)]
struct JsDecodeResult {
    ok: bool,
    /// "TomTomV3" or "Tpeg". Empty string on parse error.
    format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wkt: Option<String>,
    segments: Vec<SegmentInfo>,
    lrps: Vec<LrpInfo>,
    /// [LB, UB] of the positive offset interval. Both 0 when no pos offset.
    pos_offset_lb: f64,
    pos_offset_ub: f64,
    /// [LB, UB] of the negative offset interval. Both 0 when no neg offset.
    neg_offset_lb: f64,
    neg_offset_ub: f64,
    /// True when offset bounds were estimated from DNP sum (decode failed, path length unknown).
    /// False when exact (decode succeeded and actual path length was used).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    offsets_approximate: bool,
    /// Conservative WKT trimmed at LB (maximal coverage). Used by the copy button.
    #[serde(skip_serializing_if = "Option::is_none")]
    conservative_wkt: Option<String>,
    /// WKT of the v3 uncertainty cap at the path head (LB→UB). Absent when LB==UB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pos_uncertainty_wkt: Option<String>,
    /// WKT of the v3 uncertainty cap at the path tail (end−UB → end−LB). Absent when LB==UB.
    #[serde(skip_serializing_if = "Option::is_none")]
    neg_uncertainty_wkt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Full decode trace; null when `trace_level` is `Off` or on error.
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<serde_json::Value>,
    // ── PointAlongLine ─────────────────────────────────────────────────────────
    /// "Line" or "PointAlongLine".
    location_type: String,
    /// Decoded point coordinate for PointAlongLine — the center of the v3
    /// POFF encoding's quantization uncertainty window. Absent for line locations.
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lat: Option<f64>,
    /// Near end of that uncertainty window (offset's lower bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lon_lb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lat_lb: Option<f64>,
    /// Far end of that uncertainty window (offset's upper bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lon_ub: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lat_ub: Option<f64>,
    /// PAL orientation: "NoOrientation" | "FirstTowardSecond" | "SecondTowardFirst" | "BothDirections"
    #[serde(skip_serializing_if = "Option::is_none")]
    orientation: Option<String>,
    /// PAL side of road: "DirectlyOnOrNA" | "Right" | "Left" | "Both"
    #[serde(skip_serializing_if = "Option::is_none")]
    side_of_road: Option<String>,
}

impl JsDecodeResult {
    fn err(msg: impl Into<String>) -> Self {
        JsDecodeResult {
            ok: false,
            format: String::new(),
            wkt: None,
            segments: vec![],
            lrps: vec![],
            pos_offset_lb: 0.0,
            pos_offset_ub: 0.0,
            neg_offset_lb: 0.0,
            neg_offset_ub: 0.0,
            offsets_approximate: false,
            conservative_wkt: None,
            pos_uncertainty_wkt: None,
            neg_uncertainty_wkt: None,
            error: Some(msg.into()),
            trace: None,
            location_type: "Line".to_string(),
            point_lon: None,
            point_lat: None,
            point_lon_lb: None,
            point_lat_lb: None,
            point_lon_ub: None,
            point_lat_ub: None,
            orientation: None,
            side_of_road: None,
        }
    }
}

// ── Forced-decode snap descriptor ────────────────────────────────────────────

/// One pre-selected snap point, passed in `decode_forced()`.
#[derive(serde::Deserialize)]
struct SnapDescriptor {
    segment_id: u32,
    traversal: String,   // "Forward" or "Backward"
    arc_offset_m: f64,
    snap_lon: f64,
    snap_lat: f64,
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
    openlr_format: &'static str,
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
            openlr_format: "",
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

        let (loc_ref, fmt) = parse_openlr(openlr_string)
            .map_err(|e| JsValue::from_str(&e))?;

        let tile_keys = match loc_ref.lrps() {
            Some(lrps) => prefetch_tile_keys(lrps, &params, zoom),
            None => vec![],  // geometry types need no tiles
        };
        let tiles: Vec<[u32; 3]> = tile_keys
            .iter()
            .map(|k| [k.z as u32, k.x, k.y])
            .collect();

        self.location_ref = Some(loc_ref);
        self.params = params;
        self.zoom = zoom;
        self.openlr_format = fmt;

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
            None => return serde_json::to_string(&JsDecodeResult::err("call start() first")).unwrap(),
        };

        let result = match engine_decode(loc_ref, &self.loader.graph, &self.params, self.zoom) {
            Err(failure) => {
                // A* needs a tile that hasn't been loaded yet — not a permanent failure.
                // Return a distinct signal so JS can load the tile and retry decode().
                if let DecodeError::NeedsTile(tk) = failure.error {
                    return serde_json::to_string(&NeedsTileResult {
                        needs_tile: [tk.z as u32, tk.x, tk.y],
                    }).unwrap();
                }
                // For OffsetOverflow the route was fully found; carry the path so the JS
                // diagnostic layer can still access per-segment lengths.
                let overflow_path: Option<Vec<SegmentId>> =
                    if let DecodeError::OffsetOverflow { ref path, .. } = failure.error {
                        Some(path.clone())
                    } else {
                        None
                    };
                let error_str = failure.error.to_string();
                let trace_value = failure.trace.and_then(|t| {
                    // Fast path: serialise the whole trace at once.
                    if let Ok(val) = serde_json::to_value(&t) {
                        return Some(val);
                    }
                    // Slow path: NaN/Inf in some event field.  Serialise events one by one,
                    // dropping the offending ones.  Params are always finite, so they succeed.
                    warn("openlrlens: trace has non-finite floats; retrying per-event");
                    let n_total = t.events.len();
                    let events: Vec<serde_json::Value> = t.events.iter()
                        .filter_map(|ev| serde_json::to_value(ev).ok())
                        .collect();
                    let skipped = n_total - events.len();
                    if skipped > 0 {
                        warn(&format!("openlrlens: dropped {skipped} trace events with non-finite floats"));
                    }
                    let params_val = serde_json::to_value(&t.params)
                        .unwrap_or(serde_json::Value::Null);
                    serde_json::to_value(serde_json::json!({
                        "events": events,
                        "params": params_val,
                    })).ok()
                });
                // For OffsetOverflow: build segments from the routed path so the JS
                // diagnostic layer can access per-segment lengths even though ok=false.
                let overflow_segments: Vec<SegmentInfo> = overflow_path
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|seg_id| {
                        self.loader.graph.segments.get(seg_id).map(|seg| {
                            let (tile, local_index) = self.loader.seg_tile.get(seg_id)
                                .map(|&(z, x, y, li)| (format!("{z}/{x}/{y}"), li))
                                .unwrap_or_else(|| ("unknown".to_string(), 0));
                            SegmentInfo {
                                frc: seg.frc,
                                fow: seg.fow,
                                direction: match seg.direction {
                                    Direction::Both     => "Both",
                                    Direction::Forward  => "Forward",
                                    Direction::Backward => "Backward",
                                },
                                length_m: (seg.length_m * 10.0).round() / 10.0,
                                stable_id: seg.stable_id.clone(),
                                tile,
                                local_index,
                                segment_id: seg_id.0,
                                geometry: seg.geometry.iter()
                                    .map(|&(lon, lat)| [lon, lat])
                                    .collect(),
                            }
                        })
                    })
                    .collect();
                // Per spec §7.5.2: offset byte is relative to the first-leg DNP
                // (positive) or last-leg DNP (negative), not the total path length.
                // The second-to-last LRP holds the last leg's DNP.
                let lrps_slice = loc_ref.lrps().unwrap_or(&[]);
                let n_lrps = lrps_slice.len();
                let first_leg_dnp = lrps_slice.first().and_then(|l| l.dnp);
                let last_leg_dnp  = lrps_slice.get(n_lrps.saturating_sub(2)).and_then(|l| l.dnp);
                let (pos_offset_lb, pos_offset_ub, pos_approx) = approximate_offset(
                    lrps_slice.first().and_then(|l| l.pos_offset_raw),
                    lrps_slice.first().and_then(|l| l.pos_offset),
                    first_leg_dnp,
                );
                let (neg_offset_lb, neg_offset_ub, neg_approx) = approximate_offset(
                    lrps_slice.last().and_then(|l| l.neg_offset_raw),
                    lrps_slice.last().and_then(|l| l.neg_offset),
                    last_leg_dnp,
                );
                let full_result = JsDecodeResult {
                    lrps: lrp_info_vec(lrps_slice, &[], &[], &[]),
                    format: self.openlr_format.to_string(),
                    trace: trace_value,
                    segments: overflow_segments,
                    pos_offset_lb,
                    pos_offset_ub,
                    neg_offset_lb,
                    neg_offset_ub,
                    offsets_approximate: pos_approx || neg_approx,
                    ..JsDecodeResult::err(&error_str)
                };
                return match serde_json::to_string(&full_result) {
                    Ok(s) => s,
                    Err(e) => {
                        // LrpInfo contained a non-finite f64 — drop lrps/trace rather than panic.
                        warn(&format!("openlrlens: failure result serialisation failed ({e}); dropping lrps"));
                        serde_json::to_string(&JsDecodeResult::err(&error_str)).unwrap()
                    }
                };
            }
            Ok(r) => r,
        };

        self.build_ok_json(loc_ref, result)
    }


    /// Forced decode: bypass candidate selection and run routing with exactly the
    /// provided snap points (one per LRP).
    ///
    /// `snaps_json`: JSON array of snap descriptors:
    /// `[{ "segment_id": u32, "traversal": "Forward"|"Backward",
    ///     "arc_offset_m": f64, "snap_lon": f64, "snap_lat": f64 }, ...]`
    ///
    /// Precondition: `start()` must have been called for the current reference.
    /// The tile graph from the previous decode is reused; additional tiles are
    /// loaded on demand if A* discovers them.
    ///
    /// Returns the same JSON schema as `decode()`.
    pub fn decode_forced(&self, snaps_json: &str) -> String {
        let loc_ref = match &self.location_ref {
            Some(r) => r,
            None => return serde_json::to_string(&JsDecodeResult::err("call start() first")).unwrap(),
        };

        let snaps: Vec<SnapDescriptor> = match serde_json::from_str(snaps_json) {
            Ok(v) => v,
            Err(e) => return serde_json::to_string(
                &JsDecodeResult::err(format!("invalid snaps: {e}"))).unwrap(),
        };

        let forced_lrps = match loc_ref.lrps() {
            Some(l) => l,
            None => return serde_json::to_string(&JsDecodeResult::err(
                "decode_forced is not supported for geometry location types"
            )).unwrap(),
        };
        if snaps.len() != forced_lrps.len() {
            return serde_json::to_string(&JsDecodeResult::err(format!(
                "expected {} snaps (one per LRP), got {}", forced_lrps.len(), snaps.len()
            ))).unwrap();
        }

        let forced: Vec<ScoredCandidate> = match snaps.iter().map(|desc| {
            let seg_id = SegmentId(desc.segment_id);
            let seg = self.loader.graph.segments.get(&seg_id)
                .ok_or_else(|| format!("segment {} not in loaded graph", desc.segment_id))?;
            let traversal = match desc.traversal.as_str() {
                "Backward" => TraversalDir::Backward,
                _          => TraversalDir::Forward,
            };
            let (entry_node, exit_node) = match traversal {
                TraversalDir::Backward => (seg.end_node, seg.start_node),
                TraversalDir::Forward  => (seg.start_node, seg.end_node),
            };
            Ok(ScoredCandidate {
                segment_id: seg_id,
                traversal,
                projection: ProjectionResult {
                    arc_offset_m: desc.arc_offset_m,
                    point:        (desc.snap_lon, desc.snap_lat),
                    distance_m:   0.0,
                    bearing_deg:  0.0,
                    is_at_entry:  false,
                    is_at_exit:   false,
                },
                score: CandidateScore {
                    distance_score:       0.0,
                    bearing_score:        0.0,
                    frc_score:            0.0,
                    fow_score:            0.0,
                    interior_score:       0.0,
                    wrong_endpoint_score: 0.0,
                    total:                0.0,
                },
                entry_node,
                exit_node,
            })
        }).collect::<Result<Vec<_>, String>>() {
            Ok(v)  => v,
            Err(e) => return serde_json::to_string(&JsDecodeResult::err(e)).unwrap(),
        };

        match engine_decode_forced(loc_ref, forced, &self.loader.graph, &self.params, self.zoom) {
            Err(failure) => {
                if let DecodeError::NeedsTile(tk) = failure.error {
                    return serde_json::to_string(&NeedsTileResult {
                        needs_tile: [tk.z as u32, tk.x, tk.y],
                    }).unwrap();
                }
                let error_str = failure.error.to_string();
                let trace_value = failure.trace.and_then(|t| serde_json::to_value(t).ok());
                serde_json::to_string(&JsDecodeResult {
                    lrps: lrp_info_vec(loc_ref.lrps().unwrap_or(&[]), &[], &[], &[]),
                    format: self.openlr_format.to_string(),
                    trace: trace_value,
                    ..JsDecodeResult::err(&error_str)
                }).unwrap()
            }
            Ok(result) => self.build_ok_json(loc_ref, result),
        }
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

    /// `{ "format": ..., "location_type": ..., "lrps": [...] }` for the currently
    /// loaded reference, independent of decode success.
    ///
    /// `decode()` only includes this metadata in a result it builds itself (success
    /// or a genuine Rust-side failure) — it has no way to include it in a
    /// client-side-synthesized failure, e.g. when the JS layer gives up after
    /// exceeding its dynamic tile-load cap. Call this to enrich such a result so
    /// the UI's Reference/Trace panels have real format/location_type/lrps to show
    /// instead of falling back to placeholders.
    pub fn reference_summary(&self) -> String {
        let Some(loc_ref) = &self.location_ref else {
            return serde_json::json!({
                "format": self.openlr_format, "location_type": "Line", "lrps": [],
            }).to_string();
        };
        let lrps_slice = loc_ref.lrps().unwrap_or(&[]);
        serde_json::json!({
            "format": self.openlr_format,
            "location_type": loc_ref.type_str(),
            "lrps": lrp_info_vec(lrps_slice, &[], &[], &[]),
        }).to_string()
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

    /// Return the internal graph node ID for the node at `(z, x, y, local_index)`,
    /// or -1 if that tile/index combination is not currently loaded.
    /// Useful for correlating map-click nodes with trace log `node_id` values.
    ///
    /// Returns `f64` rather than `i64` so JS receives a plain Number (not BigInt).
    /// All node IDs are u32-bounded, so no precision is lost.
    pub fn node_id_at(&self, z: u8, x: u32, y: u32, local_index: u32) -> f64 {
        self.loader.node_tile.get(&(z, x, y, local_index))
            .map(|id| id.0 as f64)
            .unwrap_or(-1.0)
    }

    /// Return all loaded node→tile mappings as a JSON string.
    ///
    /// Each entry is `[node_id, z, x, y, local_index]`. Mirrors `all_segment_tile_mappings`,
    /// except a boundary node's global ID can appear more than once — once per tile that
    /// touches it, each at its own local index — since nodes (unlike segments) are shared
    /// across tiles rather than homed to exactly one.
    ///
    /// Used by the JS layer to build its node_id → tile reverse-lookup map.
    pub fn all_node_tile_mappings(&self) -> String {
        let mappings: Vec<[u32; 5]> = self.loader.node_tile.iter()
            .map(|(&(z, x, y, li), id)| [id.0, z as u32, x, y, li])
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

    // ── LLM diagnostic tool methods ───────────────────────────────────────────

    /// Return full attributes + geometry for one segment by its graph segment ID.
    /// Returns `{"error": "..."}` if the segment is not in the loaded tile set.
    pub fn get_segment(&self, segment_id: u32) -> String {
        let seg_id = SegmentId(segment_id);
        match self.loader.graph.segments.get(&seg_id) {
            None => serde_json::json!({
                "error": format!("segment {} not found in loaded tiles", segment_id)
            }).to_string(),
            Some(seg) => {
                let (tile, local_index) = self.loader.seg_tile.get(&seg_id)
                    .map(|&(z, x, y, li)| (format!("{z}/{x}/{y}"), li))
                    .unwrap_or_else(|| ("unknown".to_string(), 0));
                serde_json::json!({
                    "segment_id": segment_id,
                    "stable_id":  seg.stable_id,
                    "frc": seg.frc,
                    "fow": seg.fow,
                    "direction": match seg.direction {
                        Direction::Both     => "Both",
                        Direction::Forward  => "Forward",
                        Direction::Backward => "Backward",
                    },
                    "length_m":     (seg.length_m * 10.0).round() / 10.0,
                    "start_node":   seg.start_node.0,
                    "end_node":     seg.end_node.0,
                    "tile":         tile,
                    "local_index":  local_index,
                    "vertex_count": seg.geometry.len(),
                    "geometry":     seg.geometry.iter().map(|&(lon, lat)| [lon, lat]).collect::<Vec<_>>(),
                }).to_string()
            }
        }
    }

    /// Find segments in the loaded graph whose geometry comes within `radius_m` of (lat, lon).
    /// Results are sorted by distance and capped at 20.  Caps radius at 500 m.
    pub fn get_segments_near(&self, lat: f64, lon: f64, radius_m: f64) -> String {
        let cap = radius_m.min(500.0);
        let mut hits: Vec<(f64, u32, u8, u8, &'static str, f64, String)> = self.loader.graph.segments.iter()
            .filter_map(|(seg_id, seg)| {
                let min_dist = seg.geometry.iter()
                    .map(|&(slon, slat)| haversine_m(slon, slat, lon, lat))
                    .fold(f64::INFINITY, f64::min);
                if min_dist <= cap {
                    let dir_str: &'static str = match seg.direction {
                        Direction::Both     => "Both",
                        Direction::Forward  => "Forward",
                        Direction::Backward => "Backward",
                    };
                    Some((min_dist, seg_id.0, seg.frc, seg.fow, dir_str, seg.length_m, seg.stable_id.clone()))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let segments: Vec<serde_json::Value> = hits.iter().take(20).map(|(dist, id, frc, fow, dir, len, stable_id)| {
            serde_json::json!({
                "segment_id":  id,
                "stable_id":   stable_id,
                "frc":         frc,
                "fow":         fow,
                "direction":   dir,
                "length_m":    (len * 10.0).round() / 10.0,
                "distance_m":  (dist * 10.0).round() / 10.0,
            })
        }).collect();
        serde_json::json!({
            "query": { "lat": lat, "lon": lon, "radius_m": cap },
            "count": segments.len(),
            "segments": segments,
        }).to_string()
    }

    /// Return all segments connected at each endpoint of `segment_id`.
    ///
    /// Reports two groups — `at_start_node` and `at_end_node` — each listing every other
    /// segment that shares that node.  For each neighbour, `can_arrive` indicates whether a
    /// traversal of that segment can *end* at the node; `can_depart` indicates whether it can
    /// *begin* there.  Turn-restriction flags cover both transition directions through the node.
    ///
    /// This is direction-neutral and correct for bidirectional segments: a `Both` segment has
    /// two valid traversal directions, so each endpoint is simultaneously an entry and an exit.
    pub fn get_segment_neighbors(&self, segment_id: u32) -> String {
        let seg_id = SegmentId(segment_id);
        let seg = match self.loader.graph.segments.get(&seg_id) {
            Some(s) => s,
            None => return serde_json::json!({
                "error": format!("Segment {segment_id} not found in loaded graph.")
            }).to_string(),
        };

        let start_node = seg.start_node;
        let end_node   = seg.end_node;

        // Build neighbour entries for a given node id.
        // `can_arrive`  = other's traversal can end at `node`
        // `can_depart`  = other's traversal can begin at `node`
        // Both are true for Direction::Both.
        let mut build_entries = |node: NodeId| -> Vec<serde_json::Value> {
            let mut entries = Vec::new();
            for (&other_id, other) in &self.loader.graph.segments {
                if other_id == seg_id { continue; }
                let touches_node = other.start_node == node || other.end_node == node;
                if !touches_node { continue; }

                let dir_str: &'static str = match other.direction {
                    Direction::Both     => "Both",
                    Direction::Forward  => "Forward",
                    Direction::Backward => "Backward",
                };
                // Forward/Both traversal: start_node→end_node.  Departs from start_node, arrives at end_node.
                // Backward/Both traversal: end_node→start_node. Departs from end_node, arrives at start_node.
                let can_arrive = (matches!(other.direction, Direction::Forward  | Direction::Both) && other.end_node   == node)
                              || (matches!(other.direction, Direction::Backward | Direction::Both) && other.start_node == node);
                let can_depart = (matches!(other.direction, Direction::Forward  | Direction::Both) && other.start_node == node)
                              || (matches!(other.direction, Direction::Backward | Direction::Both) && other.end_node   == node);

                // Turn restrictions in both directions through this node.
                let restricted_into_self  = can_arrive  && self.loader.graph.is_restricted(other_id, node, seg_id);
                let restricted_from_self  = can_depart  && self.loader.graph.is_restricted(seg_id,   node, other_id);

                entries.push(serde_json::json!({
                    "segment_id":            other_id.0,
                    "stable_id":             other.stable_id,
                    "frc":                   other.frc,
                    "fow":                   other.fow,
                    "direction":             dir_str,
                    "length_m":              (other.length_m * 10.0).round() / 10.0,
                    "can_arrive":            can_arrive,
                    "can_depart":            can_depart,
                    "restricted_into_self":  restricted_into_self,
                    "restricted_from_self":  restricted_from_self,
                }));
            }
            entries
        };

        let at_start = build_entries(start_node);
        let at_end   = build_entries(end_node);

        serde_json::json!({
            "segment_id":  segment_id,
            "direction":   match seg.direction {
                Direction::Both     => "Both",
                Direction::Forward  => "Forward",
                Direction::Backward => "Backward",
            },
            "start_node": {
                "node_id":  start_node.0,
                "count":    at_start.len(),
                "segments": at_start,
            },
            "end_node": {
                "node_id":  end_node.0,
                "count":    at_end.len(),
                "segments": at_end,
            },
        }).to_string()
    }

    /// Re-run the decode with `params_override` merged over the current params.
    /// Tiles must already be loaded; returns an error if a new tile is required.
    /// Returns a compact comparison result — call get_decode_summary for full segment details.
    pub fn retry_decode(&mut self, params_override: &str) -> String {
        let loc_ref = match &self.location_ref {
            Some(r) => r,
            None => return serde_json::json!({"ok": false, "error": "no reference loaded; call start() first"}).to_string(),
        };
        let merged = match merge_params(&self.params, params_override) {
            Ok(p) => p,
            Err(e) => return serde_json::json!({"ok": false, "error": format!("invalid params override: {e}")}).to_string(),
        };
        // Temporarily apply merged params, decode, restore originals.
        let saved = std::mem::replace(&mut self.params, merged.clone());
        let raw = self.decode();
        self.params = saved;

        // Parse just the fields we need for a compact comparison response.
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return raw,
        };
        if parsed.get("needs_tile").is_some() {
            return serde_json::json!({
                "ok": false,
                "error": "retry requires a tile that is not loaded; re-run the full decode from the UI to load additional tiles",
                "params_applied": merged,
            }).to_string();
        }
        let ok = parsed["ok"].as_bool().unwrap_or(false);
        let seg_count = parsed["segments"].as_array().map(|a| a.len()).unwrap_or(0);
        let path_total: f64 = parsed["segments"].as_array()
            .map(|segs| segs.iter().filter_map(|s| s["length_m"].as_f64()).sum())
            .unwrap_or(0.0);
        serde_json::json!({
            "ok": ok,
            "error": parsed["error"],
            "segment_count": seg_count,
            "path_total_length_m": (path_total * 10.0).round() / 10.0,
            "lrp_count": parsed["lrps"].as_array().map(|a| a.len()),
            "params_applied": merged,
        }).to_string()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Compute approximate offset bounds for a failed decode using the relevant leg's
/// DNP as the LRP_length proxy (spec §7.5.2). Returns (lb, ub, approximate).
/// When no offset is encoded returns (0.0, 0.0, false).
/// TPEG offsets are exact even on failure (`exact` is Some, `raw` is None).
fn approximate_offset(raw: Option<u8>, exact: Option<openlr_codec::LinearInterval>,
                      leg_dnp: Option<openlr_codec::LinearInterval>) -> (f64, f64, bool) {
    if let Some(n) = raw {
        let (dnp_lb, dnp_ub) = leg_dnp.map(|d| (d.lb, d.ub)).unwrap_or((0.0, 0.0));
        let lb = n as f64 / 256.0 * dnp_lb;
        let ub = (n as f64 + 1.0) / 256.0 * dnp_ub;
        (lb, ub, true)
    } else if let Some(i) = exact {
        (i.lb, i.ub, false)
    } else {
        (0.0, 0.0, false)
    }
}

impl Decoder {
    /// Dispatch to the appropriate JSON builder based on the engine result type.
    fn build_ok_json(&self, loc_ref: &LocationReference, result: EngineDecodeResult) -> String {
        match result {
            EngineDecodeResult::GeoCoordinate { coord: (lon, lat) } =>
                serde_json::to_string(&serde_json::json!({
                    "ok": true, "format": self.openlr_format,
                    "location_type": "GeoCoordinate",
                    "point_lon": lon, "point_lat": lat,
                    "lrps": [], "segments": [],
                })).unwrap(),
            EngineDecodeResult::Circle { center: (lon, lat), radius_m } =>
                serde_json::to_string(&serde_json::json!({
                    "ok": true, "format": self.openlr_format,
                    "location_type": "Circle",
                    "center_lon": lon, "center_lat": lat, "radius_m": radius_m,
                    "lrps": [], "segments": [],
                })).unwrap(),
            EngineDecodeResult::Rectangle { lower_left, upper_right } =>
                serde_json::to_string(&serde_json::json!({
                    "ok": true, "format": self.openlr_format,
                    "location_type": "Rectangle",
                    "lower_left": lower_left, "upper_right": upper_right,
                    "lrps": [], "segments": [],
                })).unwrap(),
            EngineDecodeResult::Grid { lower_left, upper_right, n_cols, n_rows } =>
                serde_json::to_string(&serde_json::json!({
                    "ok": true, "format": self.openlr_format,
                    "location_type": "Grid",
                    "lower_left": lower_left, "upper_right": upper_right,
                    "n_cols": n_cols, "n_rows": n_rows,
                    "lrps": [], "segments": [],
                })).unwrap(),
            EngineDecodeResult::Polygon { coords } => {
                let json_coords: Vec<[f64; 2]> = coords.iter().map(|&(lon, lat)| [lon, lat]).collect();
                serde_json::to_string(&serde_json::json!({
                    "ok": true, "format": self.openlr_format,
                    "location_type": "Polygon",
                    "coords": json_coords,
                    "lrps": [], "segments": [],
                })).unwrap()
            }
            EngineDecodeResult::Line(decoded)
            | EngineDecodeResult::ClosedLine(decoded)
            | EngineDecodeResult::PointAlongLine(decoded)
            | EngineDecodeResult::PoiWithAccessPoint(decoded) =>
                self.build_network_ok_json(loc_ref, decoded),
        }
    }

    /// Build the JSON success response for a network (LRP-based) decode.
    fn build_network_ok_json(&self, loc_ref: &LocationReference, result: openlr_engine::DecodedLocation) -> String {
        let lrps = lrp_info_vec(
            loc_ref.lrps().unwrap_or(&[]),
            &result.lrp_snap_points,
            &result.lrp_snap_is_endpoint,
            &result.lrp_snap_distances_m,
        );

        let pos_int = result.pos_offset;
        let neg_int = result.neg_offset;
        let (pos_offset_lb, pos_offset_ub) = pos_int.map(|i| (i.lb, i.ub)).unwrap_or((0.0, 0.0));
        let (neg_offset_lb, neg_offset_ub) = neg_int.map(|i| (i.lb, i.ub)).unwrap_or((0.0, 0.0));

        let wkt = path_to_wkt(
            &result.path,
            pos_offset_lb,
            neg_offset_lb,
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
                    direction: match seg.direction {
                        Direction::Both     => "Both",
                        Direction::Forward  => "Forward",
                        Direction::Backward => "Backward",
                    },
                    length_m: (seg.length_m * 10.0).round() / 10.0,
                    stable_id: seg.stable_id.clone(),
                    tile,
                    local_index,
                    segment_id: seg_id.0,
                    geometry,
                }
            })
        }).collect();

        let actual_lens: Vec<f64> = result.path.iter()
            .filter_map(|id| self.loader.graph.segments.get(id))
            .map(|s| polyline_length_m(&s.geometry))
            .collect();
        let last_seg_len = actual_lens.last().copied().unwrap_or(0.0);

        let pos_uncertainty_wkt = pos_int
            .filter(|i| i.ub > i.lb)
            .and_then(|i| path_band_wkt(
                &result.path,
                result.first_lrp_arc_m + i.lb,
                result.first_lrp_arc_m + i.ub,
                result.first_seg_traversal,
                &self.loader.graph,
            ));

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

        let location_type = loc_ref.type_str().to_string();
        let (point_lon, point_lat) = result.point_coord
            .map(|(lon, lat)| (Some(lon), Some(lat)))
            .unwrap_or((None, None));
        let (point_lon_lb, point_lat_lb) = result.point_coord_lb
            .map(|(lon, lat)| (Some(lon), Some(lat)))
            .unwrap_or((None, None));
        let (point_lon_ub, point_lat_ub) = result.point_coord_ub
            .map(|(lon, lat)| (Some(lon), Some(lat)))
            .unwrap_or((None, None));
        let orientation = result.orientation.map(|o| match o {
            Orientation::NoOrientation       => "NoOrientation",
            Orientation::FirstTowardSecond   => "FirstTowardSecond",
            Orientation::SecondTowardFirst   => "SecondTowardFirst",
            Orientation::BothDirections      => "BothDirections",
        }.to_string());
        let side_of_road = result.side_of_road.map(|s| match s {
            SideOfRoad::DirectlyOnOrNA => "DirectlyOnOrNA",
            SideOfRoad::Right          => "Right",
            SideOfRoad::Left           => "Left",
            SideOfRoad::Both           => "Both",
        }.to_string());

        serde_json::to_string(&JsDecodeResult {
            ok: true,
            format: self.openlr_format.to_string(),
            wkt,
            segments,
            lrps,
            pos_offset_lb,
            pos_offset_ub,
            neg_offset_lb,
            neg_offset_ub,
            offsets_approximate: false,
            conservative_wkt: None,
            pos_uncertainty_wkt,
            neg_uncertainty_wkt,
            error: None,
            trace: trace_value,
            location_type,
            point_lon,
            point_lat,
            point_lon_lb,
            point_lat_lb,
            point_lon_ub,
            point_lat_ub,
            orientation,
            side_of_road,
        }).unwrap()
    }
}

// ── Encoder ───────────────────────────────────────────────────────────────────
//
// JS usage pattern:
//
// ```js
// import init, { Encoder } from './openlr_wasm.js';
// await init();
// const enc = new Encoder();
//
// // 1. Whenever a waypoint is placed/moved, load tiles around it.
// const { tiles } = JSON.parse(enc.tiles_near_point(lon, lat, 12));
// for (const [z, x, y] of tiles) {
//     const bytes = await pmtilesSource.getZxy(z, x, y);
//     if (bytes) enc.load_tile(z, x, y, new Uint8Array(bytes));
// }
//
// // 2. Live preview: route between waypoints as they're edited.
// const preview = JSON.parse(enc.route_between(JSON.stringify(waypoints), 12));
// if (preview.needs_tile) { /* load that tile too, retry */ }
//
// // 3. Once satisfied, encode.
// const result = JSON.parse(enc.encode_line(JSON.stringify(waypoints), 150.0, 12));
// // result: { v3, tpeg } or { needs_tile: [z,x,y] } or { error }
// ```

const WAYPOINT_SNAP_RADIUS_M: f64 = 50.0;

#[derive(serde::Deserialize)]
struct WaypointIn {
    lon: f64,
    lat: f64,
    /// Explicit disambiguation choice from `candidates_near_point` — when the
    /// click was near multiple plausible roads, snap onto exactly this
    /// segment instead of silently picking the nearest one. Ignored if
    /// `node_id` is also set (node wins — see `SnapHint`).
    #[serde(default)]
    segment_id: Option<u32>,
    /// Explicit disambiguation choice: snap directly to this intersection
    /// node instead of to a point along some road, regardless of which
    /// segment is geometrically nearest.
    #[serde(default)]
    node_id: Option<u32>,
}

/// The three ways a waypoint can resolve to a place on the graph.
enum SnapHint {
    /// Snap directly to this node (an explicit "this intersection" choice) —
    /// offset is always zero, since the user picked the junction itself, not
    /// a position along a particular road.
    Node(NodeId),
    /// Snap onto this specific segment (an explicit "this road" choice),
    /// still choosing whichever endpoint is nearer and computing a real
    /// offset — same as the default, just without the nearest-segment search.
    Segment(SegmentId),
    /// No explicit choice — pick whichever nearby segment is geometrically
    /// nearest (today's default, unambiguous-click behavior).
    Nearest,
}

impl WaypointIn {
    fn snap_hint(&self) -> SnapHint {
        if let Some(id) = self.node_id { SnapHint::Node(NodeId(id)) }
        else if let Some(id) = self.segment_id { SnapHint::Segment(SegmentId(id)) }
        else { SnapHint::Nearest }
    }
}

#[derive(Serialize)]
struct TilesResult {
    tiles: Vec<[u32; 3]>,
}

#[derive(Serialize)]
struct EncodeOutResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    v3: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tpeg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    needs_tile: Option<[u32; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl EncodeOutResult {
    fn err(msg: impl Into<String>) -> Self {
        EncodeOutResult { v3: None, tpeg: None, needs_tile: None, error: Some(msg.into()) }
    }
    fn needs_tile(tk: TileKey) -> Self {
        EncodeOutResult { v3: None, tpeg: None, needs_tile: Some([tk.z as u32, tk.x, tk.y]), error: None }
    }
}

#[derive(Serialize)]
struct RoutePreviewResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    segments: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    geometry: Option<Vec<[f64; 2]>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    length_m: Option<f64>,
    /// The actual road-network node each waypoint was snapped to, in the same
    /// order as the input waypoints — lets the UI draw a visible "offset" stub
    /// between the user's click and where the route really starts/passes
    /// through, instead of silently drawing the route as if it began exactly
    /// at the click.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapped: Option<Vec<[f64; 2]>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    needs_tile: Option<[u32; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl RoutePreviewResult {
    fn err(msg: impl Into<String>) -> Self {
        RoutePreviewResult { segments: None, geometry: None, length_m: None, snapped: None, needs_tile: None, error: Some(msg.into()) }
    }
    fn needs_tile(tk: TileKey) -> Self {
        RoutePreviewResult { segments: None, geometry: None, length_m: None, snapped: None, needs_tile: Some([tk.z as u32, tk.x, tk.y]), error: None }
    }
}

/// One nearby place a waypoint click could snap onto, for disambiguating
/// clicks that land near more than one plausible road or intersection.
/// Returned by `candidates_near_point`. `kind` is `"node"` (a real
/// intersection/junction — snapping here means an exact, zero-offset
/// anchor) or `"segment"` (a point along a road's interior, some distance
/// from either endpoint).
#[derive(Serialize)]
struct SnapCandidate {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    segment_id: Option<u32>,
    distance_m: f64,
    snapped_lon: f64,
    snapped_lat: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    frc: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fow: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stable_id: Option<String>,
}

#[derive(Serialize)]
struct CandidatesResult {
    candidates: Vec<SnapCandidate>,
}

/// A waypoint snapped onto a road segment.
struct SnappedWaypoint {
    seg_id: SegmentId,
    /// Whichever endpoint of `seg_id` is nearer (in arc-length) to the click.
    node: NodeId,
    /// Distance from `node` to the true click point, along the segment.
    offset_m: f64,
}

/// A candidate anchor for the *first or last* waypoint of a route — the two
/// boundary LRPs, which alone carry a POFF/NOFF offset in the Line format.
///
/// Unlike an interior waypoint (nearest-endpoint snapping is fine, since no
/// offset is ever recorded for it), a boundary offset is only reconstructible
/// by the decoder if it's a *forward* distance from a node the recorded path
/// genuinely starts (or ends) at, through to the click. When the click lands
/// mid-segment, that forward-reachable node is not necessarily the nearer
/// endpoint — it depends on which endpoint the rest of the route actually
/// continues from, which isn't decidable from proximity alone (see
/// `resolve_boundary_leg`, which tries both and picks whichever connects).
struct BoundaryCandidate {
    seg_id: SegmentId,
    /// Node the offset is measured from — becomes the location's start/end
    /// anchor node if this candidate wins.
    anchor: NodeId,
    /// The opposite endpoint: where the rest of the route actually connects.
    /// Equal to `anchor` (offset always 0) when the click snapped exactly
    /// onto a node, or resolved to an existing node via `SnapHint::Node`.
    continuation: NodeId,
    /// Forward distance from `anchor`, through `seg_id`, to the click.
    offset_m: f64,
}

impl BoundaryCandidate {
    /// The segment to bias the onward search against re-entering — `seg_id`
    /// when this candidate actually walks through it to reach `continuation`,
    /// or `NO_PRIOR_SEG` when `anchor == continuation` (nothing was walked).
    fn bias_seg(&self) -> SegmentId {
        if self.anchor == self.continuation { NO_PRIOR_SEG } else { self.seg_id }
    }
}

/// Every way waypoint `w` could anchor a location boundary: a single
/// zero-offset candidate for an explicit node pick, or two — one per segment
/// endpoint — for a segment/nearest-road snap, since which endpoint the path
/// actually continues from can't be decided without knowing where the route
/// goes next (the caller tries both — see `resolve_boundary_leg`).
fn boundary_candidates(graph: &Graph, w: &WaypointIn) -> Option<Vec<BoundaryCandidate>> {
    if let SnapHint::Node(node_id) = w.snap_hint() {
        if graph.nodes.contains_key(&node_id) {
            let seg_id = graph.topology_neighbors(node_id).first().map(|(_, s)| *s)?;
            return Some(vec![BoundaryCandidate { seg_id, anchor: node_id, continuation: node_id, offset_m: 0.0 }]);
        }
        // Hinted node no longer loaded — fall through to nearest-segment search.
    }
    let seg_id = match w.snap_hint() {
        SnapHint::Segment(id) if graph.segments.contains_key(&id) => id,
        _ => {
            let nearby = graph.segments_near(w.lon, w.lat, WAYPOINT_SNAP_RADIUS_M);
            nearby.into_iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?.0
        }
    };
    let seg = graph.segments.get(&seg_id)?;
    let proj = project_onto_polyline(w.lon, w.lat, &seg.geometry)?;
    let total = seg.length_m;
    Some(vec![
        BoundaryCandidate { seg_id, anchor: seg.start_node, continuation: seg.end_node, offset_m: proj.arc_offset_m },
        BoundaryCandidate { seg_id, anchor: seg.end_node, continuation: seg.start_node, offset_m: total - proj.arc_offset_m },
    ])
}

/// Try every `candidates` entry's continuation against the fixed `target`
/// node on the other side of this leg — always an interior waypoint's
/// snapped node, since a route with no interior waypoints resolves both
/// boundaries jointly instead (see `route_waypoints`). Picks whichever
/// candidate yields the shortest total distance (its own offset plus the
/// connecting search). There's no incoming leg to bias against here — this
/// is always the very first leg of the route.
fn resolve_boundary_leg(
    graph: &Graph,
    candidates: &[BoundaryCandidate],
    target: NodeId,
    zoom: u8,
) -> Result<(usize, PathResult), RouteOutcome> {
    let mut best: Option<(usize, PathResult, f64)> = None;
    for (idx, c) in candidates.iter().enumerate() {
        let (result, extra_len) = if c.continuation == target {
            (PathResult { segments: vec![], length_m: 0.0 }, 0.0)
        } else {
            match shortest_path(graph, c.continuation, c.bias_seg(), target, 7, 180.0, 0, zoom) {
                PathOutcome::Found(r) => { let l = r.length_m; (r, l) }
                PathOutcome::NeedsTile(tk) => return Err(RouteOutcome::NeedsTile(tk)),
                PathOutcome::NoPath => continue,
            }
        };
        let total = c.offset_m + extra_len;
        if best.as_ref().map_or(true, |b| total < b.2) {
            best = Some((idx, result, total));
        }
    }
    best.map(|(idx, r, _)| (idx, r))
        .ok_or_else(|| RouteOutcome::Error("no route found for a boundary waypoint".to_string()))
}

/// Mirror of `resolve_boundary_leg` for the *last* boundary: the fixed node
/// is the source (the previous leg's arrival point) and the search runs
/// forward from it to each candidate's continuation.
fn resolve_boundary_leg_from(
    graph: &Graph,
    source: NodeId,
    source_bias_seg: SegmentId,
    candidates: &[BoundaryCandidate],
    zoom: u8,
) -> Result<(usize, PathResult), RouteOutcome> {
    let mut best: Option<(usize, PathResult, f64)> = None;
    for (idx, c) in candidates.iter().enumerate() {
        let (result, extra_len) = if source == c.continuation {
            (PathResult { segments: vec![], length_m: 0.0 }, 0.0)
        } else {
            match shortest_path(graph, source, source_bias_seg, c.continuation, 7, 180.0, 0, zoom) {
                PathOutcome::Found(r) => { let l = r.length_m; (r, l) }
                PathOutcome::NeedsTile(tk) => return Err(RouteOutcome::NeedsTile(tk)),
                PathOutcome::NoPath => continue,
            }
        };
        let total = c.offset_m + extra_len;
        if best.as_ref().map_or(true, |b| total < b.2) {
            best = Some((idx, result, total));
        }
    }
    best.map(|(idx, r, _)| (idx, r))
        .ok_or_else(|| RouteOutcome::Error("no route found for a boundary waypoint".to_string()))
}

/// Snap `(lon, lat)` onto the road network per `hint` — see `SnapHint`. Used
/// for both the live-route preview and the final encode — both need "which
/// node do I route through, and how far is the true point from it" for
/// exactly the same reason `LineLocationInput` needs `start_offset_m`/
/// `end_offset_m`.
fn snap_point(graph: &Graph, lon: f64, lat: f64, hint: SnapHint) -> Option<SnappedWaypoint> {
    if let SnapHint::Node(node_id) = hint {
        if graph.nodes.contains_key(&node_id) {
            // Any touching segment works — it's never read back out except by
            // PAL, which is fine with an arbitrary-but-valid anchor line when
            // the user explicitly chose the junction itself, not a road.
            let seg_id = graph.topology_neighbors(node_id).first().map(|(_, s)| *s)?;
            return Some(SnappedWaypoint { seg_id, node: node_id, offset_m: 0.0 });
        }
        // Hinted node no longer loaded — fall through to nearest-segment search.
    }

    let seg_id = match hint {
        SnapHint::Segment(id) if graph.segments.contains_key(&id) => id,
        _ => {
            let nearby = graph.segments_near(lon, lat, WAYPOINT_SNAP_RADIUS_M);
            nearby.into_iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?.0
        }
    };
    let seg = graph.segments.get(&seg_id)?;
    let proj = project_onto_polyline(lon, lat, &seg.geometry)?;
    let total = seg.length_m;
    if proj.arc_offset_m <= total - proj.arc_offset_m {
        Some(SnappedWaypoint { seg_id, node: seg.start_node, offset_m: proj.arc_offset_m })
    } else {
        Some(SnappedWaypoint { seg_id, node: seg.end_node, offset_m: total - proj.arc_offset_m })
    }
}

/// Outcome of chaining shortest-path search across an ordered waypoint list.
enum RouteOutcome {
    Found {
        path: Vec<SegmentId>,
        start_node: NodeId,
        start_offset_m: f64,
        end_offset_m: f64,
        length_m: f64,
        /// Segment-count boundaries within `path` marking where each
        /// waypoint-to-waypoint leg ends (see `LineLocationInput::via_split_points`).
        via_split_points: Vec<usize>,
        /// The snapped node coordinate for each waypoint, in input order —
        /// see `RoutePreviewResult::snapped`.
        snapped_coords: Vec<(f64, f64)>,
    },
    NeedsTile(TileKey),
    Error(String),
}

/// Snap every waypoint, then chain `shortest_path` leg-by-leg between
/// consecutive snaps. Shared by `route_between` (map preview) and
/// `encode_line`/`encode_pal` (final encode) so both always see the exact
/// same routing decision — no stale-preview-vs-encode mismatch.
///
/// The first and last waypoints get special handling: when one snaps
/// mid-segment, its POFF/NOFF offset is only reconstructible by the decoder
/// if it's a forward distance from a node the recorded path genuinely
/// starts (or ends) at — and that's not necessarily the nearer endpoint of
/// the snapped segment. If the nearer endpoint happens to be the one the
/// route continues *away* from (its own segment never appears in the
/// recorded path at all), the offset is just a number with no path to trim
/// against, and the decoder reconstructs a bogus start/end point. Interior
/// waypoints don't have this problem — the Line format has no offset field
/// on interior LRPs, so nearest-endpoint snapping loses nothing extra.
fn route_waypoints(graph: &Graph, waypoints: &[WaypointIn], zoom: u8) -> RouteOutcome {
    if waypoints.len() < 2 {
        return RouteOutcome::Error("need at least 2 waypoints".to_string());
    }

    let first_candidates = match boundary_candidates(graph, &waypoints[0]) {
        Some(c) => c,
        None => return RouteOutcome::Error(format!(
            "no road found within {WAYPOINT_SNAP_RADIUS_M}m of waypoint 0 — load more tiles or move it closer to a road"
        )),
    };
    let last_idx = waypoints.len() - 1;
    let last_candidates = match boundary_candidates(graph, &waypoints[last_idx]) {
        Some(c) => c,
        None => return RouteOutcome::Error(format!(
            "no road found within {WAYPOINT_SNAP_RADIUS_M}m of waypoint {last_idx} — load more tiles or move it closer to a road"
        )),
    };

    if waypoints.len() == 2 {
        // The one leg spans both boundaries at once — every (first, last)
        // candidate pair is a physically distinct route, so try them all
        // jointly rather than resolving each boundary independently.
        let mut best: Option<(usize, usize, PathResult, f64)> = None;
        for (fi, fc) in first_candidates.iter().enumerate() {
            for (li, lc) in last_candidates.iter().enumerate() {
                let (result, extra_len) = if fc.continuation == lc.continuation {
                    (PathResult { segments: vec![], length_m: 0.0 }, 0.0)
                } else {
                    match shortest_path(graph, fc.continuation, fc.bias_seg(), lc.continuation, 7, 180.0, 0, zoom) {
                        PathOutcome::Found(r) => { let l = r.length_m; (r, l) }
                        PathOutcome::NeedsTile(tk) => return RouteOutcome::NeedsTile(tk),
                        PathOutcome::NoPath => continue,
                    }
                };
                let total = fc.offset_m + lc.offset_m + extra_len;
                if best.as_ref().map_or(true, |b| total < b.3) {
                    best = Some((fi, li, result, total));
                }
            }
        }
        let (fi, li, core, total) = match best {
            Some(b) => b,
            None => return RouteOutcome::Error("no route found between waypoint 0 and 1".to_string()),
        };
        let fc = &first_candidates[fi];
        let lc = &last_candidates[li];

        let mut full_path = Vec::with_capacity(core.segments.len() + 2);
        if fc.anchor != fc.continuation { full_path.push(fc.seg_id); }
        full_path.extend(core.segments);
        if lc.anchor != lc.continuation { full_path.push(lc.seg_id); }

        let snapped_coords = [fc.anchor, lc.anchor].iter()
            .filter_map(|n| graph.nodes.get(n).map(|n| (n.lon, n.lat)))
            .collect();

        return RouteOutcome::Found {
            path: full_path,
            start_node: fc.anchor,
            start_offset_m: fc.offset_m,
            end_offset_m: lc.offset_m,
            length_m: total,
            via_split_points: Vec::new(),
            snapped_coords,
        };
    }

    // Interior waypoints never carry an offset (Line format only supports
    // POFF/NOFF on the first/last LRP), so plain nearest-endpoint snapping
    // is fine — direction doesn't matter when there's no offset to
    // reconstruct.
    let mut mid_nodes = Vec::with_capacity(waypoints.len() - 2);
    for (i, w) in waypoints[1..last_idx].iter().enumerate() {
        match snap_point(graph, w.lon, w.lat, w.snap_hint()) {
            Some(s) => mid_nodes.push(s.node),
            None => return RouteOutcome::Error(format!(
                "no road found within {WAYPOINT_SNAP_RADIUS_M}m of waypoint {} — load more tiles or move it closer to a road", i + 1
            )),
        }
    }

    let (fi, first_leg) = match resolve_boundary_leg(graph, &first_candidates, mid_nodes[0], zoom) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let fc = &first_candidates[fi];

    let mut full_path: Vec<SegmentId> = Vec::new();
    let mut current_seg = fc.bias_seg();
    let mut length_m = fc.offset_m;
    let mut via_split_points = Vec::new();

    if fc.anchor != fc.continuation { full_path.push(fc.seg_id); }
    length_m += first_leg.length_m;
    if let Some(&last) = first_leg.segments.last() { current_seg = last; }
    full_path.extend(first_leg.segments);
    // `mid_nodes[0]` is always an interior waypoint here (this branch only
    // runs for len >= 3), so this leg always ends at a real via-point.
    via_split_points.push(full_path.len());

    // Interior-to-interior legs: unaffected by the boundary-offset problem
    // (no offset to reconstruct), so a plain chained search is fine. Every
    // one of these ends at another interior waypoint, so each gets a split
    // point too.
    for i in 0..mid_nodes.len() - 1 {
        match shortest_path(graph, mid_nodes[i], current_seg, mid_nodes[i + 1], 7, 180.0, 0, zoom) {
            PathOutcome::Found(r) => {
                length_m += r.length_m;
                if let Some(&last) = r.segments.last() { current_seg = last; }
                full_path.extend(r.segments);
                via_split_points.push(full_path.len());
            }
            PathOutcome::NoPath => return RouteOutcome::Error(format!(
                "no route found between waypoint {} and {}", i + 1, i + 2
            )),
            PathOutcome::NeedsTile(tk) => return RouteOutcome::NeedsTile(tk),
        }
    }

    let last_mid = *mid_nodes.last().unwrap();
    let (li, last_leg) = match resolve_boundary_leg_from(graph, last_mid, current_seg, &last_candidates, zoom) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let lc = &last_candidates[li];
    length_m += last_leg.length_m + lc.offset_m;
    full_path.extend(last_leg.segments);
    if lc.anchor != lc.continuation { full_path.push(lc.seg_id); }

    let mut snapped_coords: Vec<(f64, f64)> = Vec::with_capacity(waypoints.len());
    snapped_coords.extend(graph.nodes.get(&fc.anchor).map(|n| (n.lon, n.lat)));
    snapped_coords.extend(mid_nodes.iter().filter_map(|n| graph.nodes.get(n).map(|n| (n.lon, n.lat))));
    snapped_coords.extend(graph.nodes.get(&lc.anchor).map(|n| (n.lon, n.lat)));

    RouteOutcome::Found {
        path: full_path,
        start_node: fc.anchor,
        start_offset_m: fc.offset_m,
        end_offset_m: lc.offset_m,
        length_m,
        via_split_points,
        snapped_coords,
    }
}

/// Stateful encode session. Mirrors `Decoder`'s tile-loading lifecycle exactly
/// (same `TileLoader`), but for the opposite direction: draw waypoints, get a
/// live route preview, then encode to both physical formats.
#[wasm_bindgen]
pub struct Encoder {
    loader: TileLoader,
}

#[wasm_bindgen]
impl Encoder {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Encoder {
        Encoder { loader: TileLoader::new() }
    }

    /// Tiles to load around a newly-placed or moved waypoint (a 3x3
    /// neighborhood at `zoom`), mirroring `TileKey::neighborhood()`.
    /// Returns `{ "tiles": [[z,x,y], ...] }`.
    pub fn tiles_near_point(&self, lon: f64, lat: f64, zoom: u8) -> String {
        let tiles: Vec<[u32; 3]> = TileKey::from_lonlat(lon, lat, zoom)
            .neighborhood()
            .iter()
            .map(|k| [k.z as u32, k.x, k.y])
            .collect();
        serde_json::to_string(&TilesResult { tiles }).unwrap()
    }

    /// Inject one tile's raw OLRL bytes into the graph. Same contract as
    /// `Decoder::load_tile`.
    pub fn load_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<(), JsValue> {
        self.loader
            .load_tile_at(z, x, y, data)
            .map_err(|e| JsValue::from_str(&format!("tile parse error: {e}")))
    }

    /// Drop all loaded tiles, starting fresh.
    pub fn reset_tiles(&mut self) {
        self.loader = TileLoader::new();
    }

    /// Layer 1: snap `waypoints` onto the road network and chain shortest-path
    /// between consecutive points. `waypoints_json`:
    /// `[{"lon":...,"lat":...,"segment_id":optional}, ...]`.
    ///
    /// Returns `{ "segments": [...], "geometry": [[lon,lat],...], "snapped": [[lon,lat],...], "length_m": ... }`
    /// on success, `{ "needs_tile": [z,x,y] }` if the search needs an unloaded
    /// tile (load it and retry), or `{ "error": "..." }`.
    pub fn route_between(&self, waypoints_json: &str, zoom: u8) -> String {
        let waypoints: Vec<WaypointIn> = match serde_json::from_str(waypoints_json) {
            Ok(w) => w,
            Err(e) => return serde_json::to_string(&RoutePreviewResult::err(format!("invalid waypoints: {e}"))).unwrap(),
        };
        let result = match route_waypoints(&self.loader.graph, &waypoints, zoom) {
            RouteOutcome::Error(msg) => RoutePreviewResult::err(msg),
            RouteOutcome::NeedsTile(tk) => RoutePreviewResult::needs_tile(tk),
            RouteOutcome::Found { path, start_node, length_m, snapped_coords, .. } => {
                // Each segment's geometry is stored start_node→end_node regardless
                // of which direction the route actually traverses it — reverse it
                // when the path enters via its end_node, or the concatenated line
                // jumps straight across instead of following the road.
                let mut geometry: Vec<[f64; 2]> = Vec::new();
                let mut current = start_node;
                for seg_id in &path {
                    let Some(seg) = self.loader.graph.segments.get(seg_id) else { continue };
                    if seg.start_node == current {
                        geometry.extend(seg.geometry.iter().map(|&(lon, lat)| [lon, lat]));
                        current = seg.end_node;
                    } else {
                        geometry.extend(seg.geometry.iter().rev().map(|&(lon, lat)| [lon, lat]));
                        current = seg.start_node;
                    }
                }
                RoutePreviewResult {
                    segments: Some(path.iter().map(|s| s.0).collect()),
                    geometry: Some(geometry),
                    length_m: Some(length_m),
                    snapped: Some(snapped_coords.into_iter().map(|(lon, lat)| [lon, lat]).collect()),
                    needs_tile: None,
                    error: None,
                }
            }
        };
        serde_json::to_string(&result).unwrap()
    }

    /// Nearby road candidates a waypoint click could snap onto, for
    /// disambiguating a click near more than one plausible road (e.g. close
    /// to an intersection). Returns `{ "candidates": [{segment_id,
    /// distance_m, snapped_lon, snapped_lat, frc, fow, stable_id}, ...] }`,
    /// nearest first.
    pub fn candidates_near_point(&self, lon: f64, lat: f64) -> String {
        let graph = &self.loader.graph;
        let nearby_segs = graph.segments_near(lon, lat, WAYPOINT_SNAP_RADIUS_M);

        // Node (intersection/junction) candidates first: every distinct
        // endpoint of a nearby segment that's itself within snap radius of
        // the click. These are the "snap to this intersection" choice —
        // explicit and exact (zero offset), independent of which particular
        // road happens to be geometrically nearest.
        let mut seen_nodes: HashSet<NodeId> = HashSet::new();
        let mut candidates: Vec<SnapCandidate> = Vec::new();
        for (seg_id, _) in &nearby_segs {
            let Some(seg) = graph.segments.get(seg_id) else { continue };
            for node_id in [seg.start_node, seg.end_node] {
                if !seen_nodes.insert(node_id) { continue; }
                let Some(dist) = graph.node_dist_m(node_id, lon, lat) else { continue };
                if dist > WAYPOINT_SNAP_RADIUS_M { continue; }
                let Some(n) = graph.nodes.get(&node_id) else { continue };
                candidates.push(SnapCandidate {
                    kind: "node",
                    node_id: Some(node_id.0),
                    segment_id: None,
                    distance_m: dist,
                    snapped_lon: n.lon,
                    snapped_lat: n.lat,
                    frc: None,
                    fow: None,
                    stable_id: None,
                });
            }
        }

        // Segment (interior, along-the-road) candidates: skip any whose
        // projected point is essentially the same spot as a node candidate
        // already listed (that's not a distinct choice, it's just that
        // junction) or as another segment candidate already kept (e.g. two
        // travel-direction segments of the same road).
        let close_enough = |a_lon: f64, a_lat: f64, b_lon: f64, b_lat: f64| {
            let dx = (a_lon - b_lon) * a_lat.to_radians().cos();
            let dy = a_lat - b_lat;
            (dx * dx + dy * dy).sqrt() * 111_000.0 < 5.0
        };
        let mut seg_candidates: Vec<SnapCandidate> = nearby_segs.into_iter()
            .filter_map(|(seg_id, _dist)| {
                let seg = graph.segments.get(&seg_id)?;
                let proj = project_onto_polyline(lon, lat, &seg.geometry)?;
                Some(SnapCandidate {
                    kind: "segment",
                    node_id: None,
                    segment_id: Some(seg_id.0),
                    distance_m: proj.distance_m,
                    snapped_lon: proj.point.0,
                    snapped_lat: proj.point.1,
                    frc: Some(seg.frc),
                    fow: Some(seg.fow),
                    stable_id: Some(seg.stable_id.clone()),
                })
            })
            .collect();
        seg_candidates.sort_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap());
        for c in seg_candidates {
            let is_dup = candidates.iter().any(|d| close_enough(d.snapped_lon, d.snapped_lat, c.snapped_lon, c.snapped_lat));
            if !is_dup {
                candidates.push(c);
            }
        }

        candidates.sort_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap());
        serde_json::to_string(&CandidatesResult { candidates }).unwrap()
    }

    /// Encode `waypoints` (≥2 points) to a Line location, in both physical
    /// formats. Re-runs the same routing `route_between` does — always
    /// consistent with the last preview, never a stale cached route.
    ///
    /// `max_turn_deviation_deg` is the same turn-angle cap the decode side
    /// uses (`params.max_interior_turn_deviation_deg` — despite the name,
    /// it applies to encoding too): a combination that can only continue by
    /// doubling back across a segment it just arrived on (e.g. a real-world
    /// dead end) is rejected as `NoRoute` rather than silently encoded as a
    /// reference no real navigation system could reproduce sensibly.
    ///
    /// Returns `{ "v3": "...", "tpeg": "..." }`, `{ "needs_tile": [z,x,y] }`,
    /// or `{ "error": "..." }`.
    pub fn encode_line(&self, waypoints_json: &str, max_turn_deviation_deg: f64, zoom: u8) -> String {
        let waypoints: Vec<WaypointIn> = match serde_json::from_str(waypoints_json) {
            Ok(w) => w,
            Err(e) => return serde_json::to_string(&EncodeOutResult::err(format!("invalid waypoints: {e}"))).unwrap(),
        };
        let (path, start_node, start_offset_m, end_offset_m, via_split_points) = match route_waypoints(&self.loader.graph, &waypoints, zoom) {
            RouteOutcome::Error(msg) => return serde_json::to_string(&EncodeOutResult::err(msg)).unwrap(),
            RouteOutcome::NeedsTile(tk) => return serde_json::to_string(&EncodeOutResult::needs_tile(tk)).unwrap(),
            RouteOutcome::Found { path, start_node, start_offset_m, end_offset_m, via_split_points, .. } =>
                (path, start_node, start_offset_m, end_offset_m, via_split_points),
        };

        let input = LineLocationInput { path, start_node, start_offset_m, end_offset_m, via_split_points };
        let loc_ref = match enc_line(&self.loader.graph, &input, max_turn_deviation_deg, zoom) {
            Ok(r) => r,
            Err(openlr_encoder::EncodeError::NeedsTile(tk)) => return serde_json::to_string(&EncodeOutResult::needs_tile(tk)).unwrap(),
            Err(e) => return serde_json::to_string(&EncodeOutResult::err(e.to_string())).unwrap(),
        };
        serialize_encoded(&loc_ref)
    }

    /// Encode a single point (≥1 waypoint; only the first is used) as a
    /// PointAlongLine location. `segment_id`/`node_id`, if given, are the
    /// user's explicit disambiguation choice from `candidates_near_point`
    /// (`node_id` wins if both are set). `orientation`/`side_of_road`: same
    /// string values `Decoder`'s `JsDecodeResult` uses ("NoOrientation" |
    /// "FirstTowardSecond" | "SecondTowardFirst" | "BothDirections";
    /// "DirectlyOnOrNA" | "Right" | "Left" | "Both").
    pub fn encode_pal(&self, lon: f64, lat: f64, segment_id: Option<u32>, node_id: Option<u32>, orientation: &str, side_of_road: &str) -> String {
        let hint = match node_id {
            Some(id) => SnapHint::Node(NodeId(id)),
            None => match segment_id {
                Some(id) => SnapHint::Segment(SegmentId(id)),
                None => SnapHint::Nearest,
            },
        };
        let snap = match snap_point(&self.loader.graph, lon, lat, hint) {
            Some(s) => s,
            None => return serde_json::to_string(&EncodeOutResult::err(format!(
                "no road found within {WAYPOINT_SNAP_RADIUS_M}m of that point"
            ))).unwrap(),
        };
        let orientation = match orientation {
            "FirstTowardSecond" => Orientation::FirstTowardSecond,
            "SecondTowardFirst" => Orientation::SecondTowardFirst,
            "BothDirections" => Orientation::BothDirections,
            _ => Orientation::NoOrientation,
        };
        let side_of_road = match side_of_road {
            "Right" => SideOfRoad::Right,
            "Left" => SideOfRoad::Left,
            "Both" => SideOfRoad::Both,
            _ => SideOfRoad::DirectlyOnOrNA,
        };
        let input = PalLocationInput {
            line: snap.seg_id,
            start_node: snap.node,
            point_offset_m: snap.offset_m,
            orientation,
            side_of_road,
        };
        let loc_ref = match enc_pal(&self.loader.graph, &input) {
            Ok(r) => r,
            Err(e) => return serde_json::to_string(&EncodeOutResult::err(e.to_string())).unwrap(),
        };
        serialize_encoded(&loc_ref)
    }
}

impl Default for Encoder {
    fn default() -> Self { Self::new() }
}

fn serialize_encoded(loc_ref: &LocationReference) -> String {
    let v3 = match openlr_codec::encode_v3_base64(loc_ref) {
        Ok(s) => s,
        Err(e) => return serde_json::to_string(&EncodeOutResult::err(format!("v3 encode failed: {e}"))).unwrap(),
    };
    let tpeg = match openlr_codec::encode_tpeg_base64(loc_ref) {
        Ok(s) => s,
        Err(e) => return serde_json::to_string(&EncodeOutResult::err(format!("tpeg encode failed: {e}"))).unwrap(),
    };
    serde_json::to_string(&EncodeOutResult { v3: Some(v3), tpeg: Some(tpeg), needs_tile: None, error: None }).unwrap()
}

// ── Segment source-key helper ─────────────────────────────────────────────────

// ── Param merge helper ────────────────────────────────────────────────────────

fn merge_params(base: &DecodeParams, override_json: &str) -> Result<DecodeParams, serde_json::Error> {
    let mut base_val = serde_json::to_value(base)?;
    let overlay: serde_json::Value = serde_json::from_str(override_json)?;
    if let (Some(base_obj), Some(overlay_obj)) = (base_val.as_object_mut(), overlay.as_object()) {
        for (k, v) in overlay_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(base_val)
}

// ── Format auto-detection ─────────────────────────────────────────────────────

/// Try OpenLR binary v3 (base64) then TPEG-OLR (hex).  Returns `(LocationReference, format)`.
fn parse_openlr(s: &str) -> Result<(LocationReference, &'static str), String> {
    let has_base64_chars = s.chars().any(|c| c == '+' || c == '/' || c == '=');

    if has_base64_chars || looks_like_base64(s) {
        if let Ok(r) = decode_v3_base64(s)   { return Ok((r, "TomTomV3")); }
        if let Ok(r) = decode_tpeg_base64(s) { return Ok((r, "Tpeg")); }
    }

    if let Ok(r) = decode_tpeg_hex(s) { return Ok((r, "Tpeg")); }

    decode_v3_base64(s)
        .map(|r| (r, "TomTomV3"))
        .map_err(|e| format!("OpenLR parse error (tried v3 base64, TPEG base64, TPEG hex): {e}"))
}

fn looks_like_base64(s: &str) -> bool {
    // Heuristic: all chars are base64url-safe, and length is 4-byte aligned (with or without padding).
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && s.len() % 4 == 0
}
