import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import { decodeTile } from './tileDecoder.js';
import { buildReplaySteps } from './replayEngine.js';
import { loadLlmConfig, saveLlmConfig, clearLlmConfig as clearLlmStorage, chatComplete } from './llmClient.js';
import { buildSystemContext } from './llmDiagnosis.js';
import { TOOL_DEFINITIONS, ENCODE_TOOL_DEFINITIONS, executeTool } from './llm/tools.js';

let _pmtiles = null;
let _decoder = null;
let _encoder = null;
let _zoom = 12;

// The pmtiles JS library auto-decompresses tile data before returning it from
// getZxy(), so res.data is always raw (uncompressed) bytes regardless of the
// archive's tile_compression field.
function tileBytes(res) {
  if (!res?.data) return null;
  return new Uint8Array(res.data);
}

/** Build a client-side decode-failure result (e.g. giving up after exceeding the
 *  dynamic tile-load cap) that still has real format/location_type/lrps to show —
 *  decode()'s own Rust-side failures include this metadata, but a failure invented
 *  entirely in JS has no way to without asking the decoder for it separately. */
function clientDecodeFailure(message) {
  let summary = { format: '', location_type: 'Line', lrps: [] };
  try {
    if (_decoder) summary = JSON.parse(_decoder.reference_summary());
  } catch { /* decoder unavailable or reference_summary() missing — use fallback */ }
  return { ok: false, error: message, segments: [], ...summary };
}
/** Build an `encodeResult` failure object from a raw `_encoder.encode_line`/
 *  `encode_pal` JSON result, carrying through the optional structured leg
 *  context (`error_from_node`/`error_to_node`/`error_from_segment_id`) that
 *  `Encoder::diagnose_connection` needs — only set on waypoint-to-waypoint
 *  connection failures, null otherwise. */
function encodeErrorResult(out) {
  return {
    v3: null, tpeg: null, error: out.error,
    error_from_node: out.error_from_node ?? null,
    error_to_node: out.error_to_node ?? null,
    error_from_segment_id: out.error_from_segment_id ?? null,
  };
}

/** segment_id → { tile_key, local_index } — rebuilt after every decode */
let _segIdToTile = new Map();
/** tile_key → GeoJSON features[] — built from tile bytes during decode */
let _tileGeomCache = new Map();
/** segment_id → GeoJSON feature — direct lookup, built from the two caches above */
let _segGeomCache = new Map();

export function setPmtiles(p) { _pmtiles = p; }
export function setDecoder(d) { _decoder = d; }
export function setEncoder(e) { _encoder = e; }
export function setZoom(z)    { _zoom = z; }

/** Load the 3x3 tile neighborhood around a waypoint into the encoder's graph.
 *  Best-effort prefetch — runLiveRoute()/runEncode()'s own needs_tile retry
 *  loop covers anything this misses (e.g. a route leg passing through tiles
 *  far from either endpoint). */
export async function loadEncoderTilesNear(lon, lat) {
  if (!_encoder || !_pmtiles) return;
  const { tiles } = JSON.parse(_encoder.tiles_near_point(lon, lat, _zoom));
  await Promise.all(tiles.map(async ([z, x, y]) => {
    try {
      const res = await _pmtiles.getZxy(z, x, y);
      const bytes = tileBytes(res);
      _encoder.load_tile(z, x, y, bytes ?? new Uint8Array(0));
    } catch { /* best-effort; needs_tile retry loop covers gaps */ }
  }));
}

/** Nearby distinct road candidates a click could snap onto — used to decide
 *  whether to show a disambiguation picker before committing a waypoint.
 *  Returns [] if the encoder isn't ready or nothing is within snap radius. */
export function getSnapCandidates(lon, lat) {
  if (!_encoder) return [];
  try {
    return JSON.parse(_encoder.candidates_near_point(lon, lat)).candidates ?? [];
  } catch {
    return [];
  }
}

/** Stateless route preview — same underlying call `runLiveRoute` uses, but
 *  touches nothing in the store (`waypoints`/`liveRoute`/`waypointHistory`
 *  are all untouched). Used by the encode popup to show what the route
 *  would look like for a candidate choice *before* committing to it, so
 *  clicking through several candidates never pollutes undo history the way
 *  calling the real add/insert/move actions repeatedly would. Returns null
 *  on error or if a needed tile still isn't loaded after one retry (a
 *  preview silently not appearing is fine; committing via Enter still goes
 *  through the real, fully-retrying code path). */
export async function previewRouteBetween(waypointsList, maxTurnDeviationDeg) {
  if (!_encoder) return null;
  const tryOnce = () => {
    try { return JSON.parse(_encoder.route_between(JSON.stringify(waypointsList), maxTurnDeviationDeg, _zoom)); }
    catch { return null; }
  };
  let out = tryOnce();
  if (out?.needs_tile && _pmtiles) {
    const [z, x, y] = out.needs_tile;
    try {
      const res = await _pmtiles.getZxy(z, x, y);
      _encoder.load_tile(z, x, y, tileBytes(res) ?? new Uint8Array(0));
      out = tryOnce();
    } catch { return null; }
  }
  if (!out || out.error || out.needs_tile) return null;
  return out;
}

/** Full attributes + geometry for one segment from the *encoder's* loaded
 *  graph, by internal segment ID — used to build the live per-segment table
 *  in the encode-mode Results panel while the route is still just a
 *  preview (no verify-decode has run yet to populate the usual caches). */
export function getEncoderSegment(segId) {
  if (!_encoder) return null;
  try { return JSON.parse(_encoder.get_segment(segId)); } catch { return null; }
}

export function getSegIdToTile()   { return _segIdToTile; }
export function getTileGeomCache() { return _tileGeomCache; }
export function getSegGeomCache()  { return _segGeomCache; }

/** Look up the internal graph segment ID by tile + local index.  Returns -1 when
 *  the tile hasn't been loaded by the decoder yet (e.g. before the first decode). */
export function getSegmentId(z, x, y, localIndex) {
  if (!_decoder) return -1;
  return _decoder.segment_id_at(z, x, y, localIndex);
}

/** Look up the internal graph node ID by tile + local index.  Returns -1 when
 *  the tile hasn't been loaded by the decoder yet (e.g. before the first decode). */
export function getNodeId(z, x, y, localIndex) {
  if (!_decoder) return -1;
  return _decoder.node_id_at(z, x, y, localIndex);
}

function defaultFrcTable() {
  const p = [0.00, 0.10, 0.25, 0.45, 0.65, 0.80, 0.90, 1.00];
  return Array.from({ length: 8 }, (_, i) =>
    Array.from({ length: 8 }, (_, j) => p[Math.abs(i - j)])
  );
}

const DEFAULT_FOW_TABLE = [
  [0.00, 0.30, 0.30, 0.30, 0.30, 0.30, 0.30, 0.30],
  [0.30, 0.00, 0.10, 0.40, 0.60, 0.70, 0.20, 0.80],
  [0.30, 0.10, 0.00, 0.20, 0.40, 0.50, 0.25, 0.70],
  [0.30, 0.40, 0.20, 0.00, 0.20, 0.25, 0.30, 0.40],
  [0.30, 0.60, 0.40, 0.20, 0.00, 0.30, 0.40, 0.50],
  [0.30, 0.70, 0.50, 0.25, 0.30, 0.00, 0.50, 0.40],
  [0.30, 0.20, 0.25, 0.30, 0.40, 0.50, 0.00, 0.50],
  [0.30, 0.80, 0.70, 0.40, 0.50, 0.40, 0.50, 0.00],
];

export const PRESETS = {
  Permissive: {
    candidate_search_radius_m:    200.0,
    snap_to_endpoint_threshold_m:  25.0,
    distance_weight:                0.5,
    bearing_weight:                 0.2,
    bearing_penalty_per_bucket:     0.03,
    frc_weight:                     0.05,
    fow_weight:                     0.10,
    interior_weight:                0.05,
    wrong_endpoint_weight:          0.10,
    frc_penalty_table: defaultFrcTable(),
    fow_penalty_table: DEFAULT_FOW_TABLE,
    max_bearing_deviation_deg:     90.0,
    max_candidate_score:            1.5,
    max_candidates_per_lrp:        10,
    dnp_tolerance_pct:              0.40,
    max_path_search_factor:         4.0,
    max_astar_expansions:       50000,
    lfrcnp_tolerance:               2,
    max_interior_turn_deviation_deg: 180.0,
    max_routing_attempts:           0,
    trace_level: 'Summary',
  },
  Default: {
    candidate_search_radius_m:     30.0,
    snap_to_endpoint_threshold_m:  15.0,
    distance_weight:                0.5,
    bearing_weight:                 0.3,
    bearing_penalty_per_bucket:     0.05,
    frc_weight:                     0.10,
    fow_weight:                     0.20,
    interior_weight:                0.10,
    wrong_endpoint_weight:          5.00,
    frc_penalty_table: defaultFrcTable(),
    fow_penalty_table: DEFAULT_FOW_TABLE,
    max_bearing_deviation_deg:     45.0,
    max_candidate_score:            1.5,
    max_candidates_per_lrp:         8,
    dnp_tolerance_pct:              0.25,
    max_path_search_factor:         5.0,
    max_astar_expansions:      100000,
    lfrcnp_tolerance:               2,
    max_interior_turn_deviation_deg: 150.0,
    max_routing_attempts:          10,
    trace_level: 'Summary',
  },
  Strict: {
    candidate_search_radius_m:     50.0,
    snap_to_endpoint_threshold_m:  10.0,
    distance_weight:                0.5,
    bearing_weight:                 0.4,
    bearing_penalty_per_bucket:     0.08,
    frc_weight:                     0.20,
    fow_weight:                     0.30,
    interior_weight:                0.20,
    wrong_endpoint_weight:          0.30,
    frc_penalty_table: defaultFrcTable(),
    fow_penalty_table: DEFAULT_FOW_TABLE,
    max_bearing_deviation_deg:     30.0,
    max_candidate_score:            1.0,
    max_candidates_per_lrp:         5,
    dnp_tolerance_pct:              0.10,
    max_path_search_factor:         3.0,
    max_astar_expansions:           0,
    lfrcnp_tolerance:               0,
    max_interior_turn_deviation_deg: 120.0,
    max_routing_attempts:           5,
    trace_level: 'Summary',
  },
};

// Produce a human-readable label for a tool call, incorporating key arguments.
function toolCallLabel(name, args) {
  switch (name) {
    case 'get_lrp_candidates': return `get_lrp_candidates(${args.lrp_index ?? '?'})`;
    case 'get_leg_summary':    return `get_leg_summary(${args.leg_index ?? '?'})`;
    case 'get_route_segments': return `get_route_segments(${args.leg_index ?? '?'})`;
    default: return name;
  }
}

// Summarise an array of tool call records into the shape stored in llmLastToolActivity.
function buildToolActivity(calls) {
  return {
    calls,
    total_result_bytes: calls.reduce((s, c) => s + c.result_bytes, 0),
  };
}

export const useStore = create(persist(
 (set, get) => ({
  openlrString: '',
  tileUrl: 'http://localhost:5176',
  params: { ...PRESETS.Default },
  showParams: false,
  showLlmSettings: false,
  showTrace: false,
  showResult: false,
  showReplay: false,
  llmConfig: loadLlmConfig(),
  llmChatOpen: false,
  llmMessages: [],       // display: { role, content, display?, error? }
  llmApiHistory: [],     // api: full history including tool call/result turns (not shown in UI)
  llmLastToolActivity: null, // { calls: [{label, result_bytes}], total_bytes } for last exchange
  llmLoading: false,
  llmStreamingContent: null, // string while final response is streaming, null otherwise
  showSegmentLayer: false,
  decoding: false,
  decodeResult: null,
  decodeToast: null,         // { message: string } | null; set on failure, cleared by component
  forcedDecoding: false,
  forcedDecodeResult: null,   // result from decode_forced(), null until user runs it
  pinnedCandidates: {},       // { [lrpIdx]: { segment_id, traversal, arc_offset_m, snap_lon, snap_lat } | null }
  savedParamSets: {},      // { [name: string]: DecodeParams }
  highlightedSegment: null,
  traceHighlightSegIds: null,
  traceHighlightSnaps: null,   // { from: [lon,lat], to: [lon,lat] } when highlighting a leg route
  traceLrpFocus: null,
  mapFlyTo: null,           // { lat, lon, zoom, _tick } — consumed by Map to call map.flyTo()
  candidatePopup: null,
  // ── Replay state ─────────────────────────────────────────────────────────
  replaySteps: [],        // pre-built display steps from buildReplaySteps()
  replayStats: null,      // { maxG, totalNodes, phases }
  replayStep: 0,          // current display step index

  // ── Encode mode state ────────────────────────────────────────────────────
  mode: 'decode',           // 'decode' | 'encode'
  // 'Line' and 'PointAlongLine' are implemented; the other 7 OpenLR location
  // types are listed in the UI (disabled) for discoverability but have no
  // encoder support yet.
  locationType: 'Line',
  // PointAlongLine-only encode options — set from either the map's
  // right-click popup or the Results panel, both read/write the same store
  // fields so whichever one you touch last is what encode() actually uses.
  palOrientation: 'NoOrientation',
  palSideOfRoad:  'DirectlyOnOrNA',
  // Rule-1 cap, meters — encoder-only (nothing on decode depends on it; tile
  // prefetch and A*'s search radius are already sized from the real per-leg
  // DNP the wire data carries). Lower it to keep encoded references leg-sized
  // for e.g. a memory-constrained decoder's smaller per-leg tile budget.
  // Clamped server-side to the architecture's own 15km ceiling.
  maxEncodeLegM: 15_000,
  waypoints: [],            // ordered [{lon,lat}], user-drawn
  waypointHistory: [],      // undo stack: prior `waypoints` snapshots
  liveRoute: null,          // { segments, geometry, length_m } from route_between()
  liveRouteError: null,
  liveRouteLoading: false,
  encoding: false,
  encodeResult: null,       // { v3, tpeg, error } | null
  // Round-trip verification: the freshly-encoded location, decoded straight back
  // via the existing _decoder. Mirrors decodeResult/replaySteps/replayStats
  // shape exactly so Results/Trace/Replay panels can read this in encode mode.
  verifyResult: null,
  verifyToast: null,
  verifyReplaySteps: [],
  verifyReplayStats: null,
  verifyReplayStep: 0,

  setOpenlrString: (s) => set({ openlrString: s }),
  setTileUrl: (url) => set({ tileUrl: url }),

  resetToDefaults: () => set({ params: { ...PRESETS.Default } }),

  loadPreset: (name) => set({ params: { ...PRESETS[name] } }),

  saveParamSet: (name, params) => set(state => ({
    savedParamSets: { ...state.savedParamSets, [name]: { ...params } },
  })),
  deleteParamSet: (name) => set(state => {
    const next = { ...state.savedParamSets };
    delete next[name];
    return { savedParamSets: next };
  }),
  loadParamSet: (name) => set(state => ({
    params: { ...state.savedParamSets[name] },
  })),

  setParam: (key, value) => set(state => ({
    params: { ...state.params, [key]: value },
  })),

  setTraceLevel: (level) => set(state => ({
    params: { ...state.params, trace_level: level },
  })),

  setTableCell: (tableKey, row, col, value) => set(state => {
    const table = state.params[tableKey].map(r => [...r]);
    table[row][col] = value;
    return { params: { ...state.params, [tableKey]: table } };
  }),

  toggleParams:        () => set(state => ({ showParams:        !state.showParams })),
  toggleLlmSettings:   () => set(state => ({ showLlmSettings:   !state.showLlmSettings })),

  setLlmConfig: (config) => { saveLlmConfig(config); set({ llmConfig: config }); },
  clearLlmConfig: () => { clearLlmStorage(); set({ llmConfig: null }); },

  toggleLlmChat: () => set(s => ({ llmChatOpen: !s.llmChatOpen })),
  clearLlmChat:  () => set({ llmMessages: [], llmApiHistory: [], llmLastToolActivity: null, llmLoading: false, llmStreamingContent: null }),

  // content = text sent to the API (may include appended format hints)
  // display = text shown in the chat bubble (the user's original words)
  sendLlmMessage: async (content, display) => {
    const {
      llmMessages, llmApiHistory, decodeResult, params, llmConfig, mode,
      encodeResult, verifyResult, waypoints, liveRoute, maxEncodeLegM, locationType,
    } = get();
    const isEncode = mode === 'encode';
    if (!llmConfig || (isEncode ? !encodeResult : !decodeResult)) return;

    // In encode mode the "decode result" the chat and its tools reason about
    // is the round-trip verify decode (a real DecodeResult from a real
    // decode), not the raw encodeResult — this is what lets every existing
    // decode-side tool (get_decode_summary, get_lrp_candidates, ...) work
    // unmodified once an encode succeeds and gets verified.
    const activeDecodeResult = isEncode ? verifyResult : decodeResult;

    const userDisplayMsg = { role: 'user', content, display: display ?? content };
    set({ llmMessages: [...llmMessages, userDisplayMsg], llmLastToolActivity: null, llmLoading: true });

    // Rebuild system context each turn so parameter changes are reflected immediately
    const systemContext = buildSystemContext(mode, decodeResult, params, {
      encodeResult, verifyResult, waypoints, liveRoute, params, maxEncodeLegM, locationType,
    });

    // Decode tools only make sense once there's something to decode — skip
    // advertising all 22 of them while an encode is still failing (token
    // economy: an unused tool schema is still prompt tokens every turn).
    const tools = isEncode
      ? [...ENCODE_TOOL_DEFINITIONS, ...(verifyResult ? TOOL_DEFINITIONS : [])]
      : TOOL_DEFINITIONS;

    // Cap history at 20 entries (~10 exchange pairs) to bound context window growth.
    // The model can re-call tools if it needs data that has aged out.
    const MAX_API_HISTORY = 20;
    const rawHistory = llmApiHistory.slice(-MAX_API_HISTORY);
    // Trim to a clean conversation boundary: never start with an orphaned tool result.
    // A clean start is a plain user message (string content, not a tool-result array).
    let trimStart = 0;
    for (let i = 0; i < rawHistory.length; i++) {
      const m = rawHistory[i];
      if (m.role === 'user' && typeof m.content === 'string') {
        trimStart = i;
        break;
      }
    }
    const trimmedHistory = rawHistory.slice(trimStart);

    // apiHistory is the full multi-turn API conversation (includes tool call/result turns)
    let apiHistory = [
      { role: 'system', content: systemContext },
      ...trimmedHistory,
      { role: 'user', content },
    ];

    // Track new entries added this turn so we can persist them after the loop
    const newApiEntries = [{ role: 'user', content }];
    // Accumulate tool call activity for the strip display
    const toolCalls = [];

    // onDelta streams text into llmStreamingContent as the final response arrives.
    // Tool-call steps may also stream text (brief preamble) which gets cleared when
    // tools are detected.
    const onDelta = (chunk) => {
      set(s => ({ llmStreamingContent: (s.llmStreamingContent ?? '') + chunk }));
    };

    const MAX_STEPS = 20;
    for (let step = 0; step < MAX_STEPS; step++) {
      set({ llmStreamingContent: null }); // typing dots until first text chunk
      const resp = await chatComplete(llmConfig, apiHistory, tools, onDelta);

      if (!resp.ok) {
        set(s => ({
          llmMessages: [...s.llmMessages, { role: 'assistant', content: resp.error ?? 'Unknown error', error: true }],
          llmLastToolActivity: toolCalls.length ? buildToolActivity(toolCalls) : null,
          llmLoading: false,
          llmStreamingContent: null,
        }));
        return;
      }

      if (!resp.tool_calls?.length) {
        // Final text response — streaming content is already displayed; commit to history
        const assistantMsg = { role: 'assistant', content: resp.content ?? '' };
        const finalApiEntry = { role: 'assistant', content: resp.content ?? '' };
        set(s => ({
          llmMessages: [...s.llmMessages, assistantMsg],
          llmApiHistory: [...llmApiHistory, ...newApiEntries, finalApiEntry],
          llmLastToolActivity: toolCalls.length ? buildToolActivity(toolCalls) : null,
          llmLoading: false,
          llmStreamingContent: null,
        }));
        return;
      }

      // Tool-call step: clear any streamed preamble text; pre-populate strip
      // with pending calls so the user sees them before execution starts
      const pendingCalls = resp.tool_calls.map(tc => {
        let args = {};
        try { args = JSON.parse(tc.function.arguments); } catch {}
        return { label: toolCallLabel(tc.function.name, args), args_bytes: tc.function.arguments.length, result_bytes: 0, pending: true };
      });
      set({ llmStreamingContent: null, llmLastToolActivity: buildToolActivity([...toolCalls, ...pendingCalls]) });

      // Tool-use round: add assistant tool-call message to history and execute each tool
      const assistantApiEntry = {
        role: 'assistant',
        content: resp.content ?? null,
        tool_calls: resp.tool_calls,
      };
      newApiEntries.push(assistantApiEntry);
      apiHistory = [...apiHistory, assistantApiEntry];

      const storeActions = {
        setPinnedCandidates: (snapsArray) => {
          get().clearPinnedCandidates();
          snapsArray.forEach(({ lrp_index, ...snap }) => get().setPinnedCandidate(lrp_index, snap));
        },
        runForcedDecodeAndGet: async () => {
          await get().runForcedDecode();
          return get().forcedDecodeResult;
        },
        highlightSegments: (segIds) => get().setTraceHighlight(segIds),
        flyTo: (lat, lon, zoom) => get().setMapFlyTo(lat, lon, zoom),
      };

      for (const tc of resp.tool_calls) {
        let toolResult;
        try {
          const args = JSON.parse(tc.function.arguments);
          const forcedDecodeResult = get().forcedDecodeResult;
          toolResult = await executeTool(tc.function.name, args, {
            decodeResult: activeDecodeResult, params, decoder: _decoder, storeActions, forcedDecodeResult,
            encoder: _encoder, encodeResult, waypoints, liveRoute, maxEncodeLegM, zoom: _zoom,
          });
          toolCalls.push({
            label: toolCallLabel(tc.function.name, args),
            args_bytes: tc.function.arguments.length,
            result_bytes: toolResult.length,
          });
        } catch (e) {
          toolResult = JSON.stringify({ error: e.message });
          toolCalls.push({ label: tc.function.name, args_bytes: 0, result_bytes: toolResult.length });
        }
        // Update strip in real-time so each tool appears as it completes
        set({ llmLastToolActivity: buildToolActivity(toolCalls) });
        const toolApiEntry = { role: 'tool', tool_call_id: tc.id, content: toolResult };
        newApiEntries.push(toolApiEntry);
        apiHistory = [...apiHistory, toolApiEntry];
      }
    }

    // Reached max steps without a final answer
    set(s => ({
      llmMessages: [...s.llmMessages, { role: 'assistant', content: '[Max tool call steps reached without a final response]', error: true }],
      llmLastToolActivity: toolCalls.length ? buildToolActivity(toolCalls) : null,
      llmLoading: false,
      llmStreamingContent: null,
    }));
  },
  toggleTrace:         () => set(state => ({ showTrace:         !state.showTrace })),
  toggleReplay:        () => set(state => ({ showReplay:        !state.showReplay })),
  toggleSegmentLayer:  () => set(state => ({ showSegmentLayer:  !state.showSegmentLayer })),

  // Both mode-aware: in encode mode these drive the round-trip verify-decode's
  // replay data instead of the last manual decode's.
  setReplayStep: (n) => set(state => state.mode === 'encode'
    ? { verifyReplayStep: Math.max(0, Math.min(n, state.verifyReplaySteps.length - 1)) }
    : { replayStep:       Math.max(0, Math.min(n, state.replaySteps.length - 1)) }),
  stepReplay: (delta) => set(state => state.mode === 'encode'
    ? { verifyReplayStep: Math.max(0, Math.min(state.verifyReplayStep + delta, state.verifyReplaySteps.length - 1)) }
    : { replayStep:       Math.max(0, Math.min(state.replayStep + delta, state.replaySteps.length - 1)) }),

  // Re-decode at an elevated trace level and open the trace panel.
  // Off → Summary on first call; Summary or Full → Full on subsequent calls.
  debugDecode: async () => {
    const { params } = get();
    const current = params.trace_level ?? 'Summary';
    const elevated = current === 'Off' ? 'Summary' : 'Full';
    set(state => ({ params: { ...state.params, trace_level: elevated }, showTrace: true }));
    await get().runDecode();
  },

  // Same idea as debugDecode, but re-runs the encode-mode round-trip verify
  // decode instead of the manual openlrString decode.
  debugVerify: async () => {
    const { params, encodeResult } = get();
    if (!encodeResult?.v3) return;
    const current = params.trace_level ?? 'Summary';
    const elevated = current === 'Off' ? 'Summary' : 'Full';
    set(state => ({ params: { ...state.params, trace_level: elevated }, showTrace: true }));
    await get().runVerifyDecode(encodeResult.v3);
  },

  hideResult:    () => set({ showResult: false }),
  openResult:    () => set({ showResult: true }),
  toggleResult:  () => set(state => ({ showResult: !state.showResult })),
  clearDecodeToast: () => set({ decodeToast: null }),
  // "Clear" button next to Decode — wipes the input and everything a decode
  // produced (map layers follow decodeResult:null automatically), so the
  // user gets a blank slate rather than a stale route left on screen.
  clearResult: () => set({
    openlrString: '',
    decodeResult: null,
    decodeToast: null,
    showResult: false,
    showTrace: false,
    showReplay: false,
    highlightedSegment: null,
    traceHighlightSegIds: null,
    traceHighlightSnaps: null,
    traceLrpFocus: null,
    candidatePopup: null,
    replaySteps: [],
    replayStats: null,
    replayStep: 0,
    pinnedCandidates: {},
    forcedDecodeResult: null,
    llmMessages: [],
    llmApiHistory: [],
    llmLoading: false,
  }),
  setHighlightedSegment: (seg) => set({ highlightedSegment: seg }),
  // Request the segment info popup to open for a given tile+local_index.
  // Map.jsx watches this and opens the popup; call clearRequestedInfoSegment() after handling.
  requestedInfoSegment: null,
  requestInfoSegment:      (tile, local_index) => set({ requestedInfoSegment: { tile, local_index } }),
  clearRequestedInfoSegment: () => set({ requestedInfoSegment: null }),
  setTraceHighlight: (ids, snaps) => set({ traceHighlightSegIds: ids?.length ? ids : null, traceHighlightSnaps: snaps ?? null }),
  setCandidatePopup: (data) => set({ candidatePopup: data }),
  clearCandidatePopup: () => set({ candidatePopup: null }),
  setTraceLrpFocus: (lrp) => set({ traceLrpFocus: lrp ? { ...lrp, _tick: Date.now() } : null }),
  setMapFlyTo: (lat, lon, zoom) => set({ mapFlyTo: { lat, lon, zoom, _tick: Date.now() } }),

  setPinnedCandidate: (lrpIdx, snap) => set(state => ({
    pinnedCandidates: { ...state.pinnedCandidates, [lrpIdx]: snap ?? null },
    forcedDecodeResult: null,  // invalidate when pins change
  })),

  clearPinnedCandidates: () => set({ pinnedCandidates: {}, forcedDecodeResult: null }),

  runForcedDecode: async () => {
    const { decodeResult, pinnedCandidates, params } = get();
    if (!_decoder || !decodeResult) return;

    const lrpCount = decodeResult.lrps?.length ?? 0;
    const snaps = Array.from({ length: lrpCount }, (_, i) => pinnedCandidates[i]);
    if (snaps.some(s => !s)) return;  // not all LRPs pinned

    set({ forcedDecoding: true, forcedDecodeResult: null });
    try {
      const snapsJson = JSON.stringify(snaps);
      const attemptedTiles = new Set();
      const MAX_DYNAMIC_LOADS = 10;
      let dynamicLoads = 0;
      let result = null;
      while (true) {
        result = JSON.parse(_decoder.decode_forced(snapsJson));
        if (!result.needs_tile) break;

        if (dynamicLoads >= MAX_DYNAMIC_LOADS) {
          const msg = `Forced decode exceeded the maximum of ${MAX_DYNAMIC_LOADS} dynamically-loaded tiles. Try reducing the path search factor first: a smaller value often lets a decode complete (with real trace data to diagnose the actual cause) instead of repeatedly restarting the search on unrelated dead-end tiles.`;
          console.warn(`[forced-decode] ${msg}`);
          result = clientDecodeFailure(msg);
          break;
        }

        const [z, x, y] = result.needs_tile;
        const tileKey = `${z}/${x}/${y}`;
        if (attemptedTiles.has(tileKey)) {
          const msg = `A* re-requested tile ${tileKey} that was already loaded — this indicates an internal bug`;
          console.warn(`[forced-decode] ${msg}`);
          result = clientDecodeFailure(msg);
          break;
        }
        attemptedTiles.add(tileKey);
        dynamicLoads++;
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          const bytes = tileBytes(res);
          _decoder.load_tile(z, x, y, bytes ?? new Uint8Array(0));
        } catch (e) {
          const msg = `Failed to load tile ${tileKey}: ${e?.message ?? e}`;
          console.warn(`[forced-decode] ${msg}`);
          result = clientDecodeFailure(msg);
          break;
        }
      }
      // Enrich segments with stable_id from tile geometry cache
      for (const seg of result.segments ?? []) {
        const feat = _segGeomCache.get(seg.segment_id);
        if (feat) seg.stable_id = feat.properties.stable_id ?? null;
      }

      // Splice replay: candidate events from original trace, routing events from forced trace.
      const originalEvents = get().decodeResult?.trace?.events ?? [];
      const forcedEvents   = result.trace?.events ?? [];
      const firstRouteOrig  = originalEvents.findIndex(e => e.RouteSearchStarted != null);
      const firstRouteForced = forcedEvents.findIndex(e => e.RouteSearchStarted != null);
      const candidateEvents = firstRouteOrig  >= 0 ? originalEvents.slice(0, firstRouteOrig)  : originalEvents;
      const routingEvents   = firstRouteForced >= 0 ? forcedEvents.slice(firstRouteForced)      : [];
      const splicedEvents   = [...candidateEvents, ...routingEvents];
      const replayData = splicedEvents.length
        ? buildReplaySteps(splicedEvents)
        : { steps: [], stats: { maxG: 0, totalNodes: 0, phases: [] } };

      set({
        forcedDecoding: false,
        forcedDecodeResult: result,
        replaySteps: replayData.steps,
        replayStats: replayData.stats,
        replayStep:  0,
      });
    } catch (e) {
      set({ forcedDecoding: false, forcedDecodeResult: clientDecodeFailure(String(e)) });
    }
  },

  runDecode: async () => {
    const { openlrString, params } = get();
    if (!openlrString.trim() || !_pmtiles || !_decoder) return;

    set(state => ({
      decoding: true,
      decodeResult: null,
      // Reset transient UI state for the new decode; preserve showResult so an
      // open panel stays open rather than collapsing and re-expanding (flicker).
      showTrace: false,
      showReplay: false,
      showSegmentLayer: false,
      highlightedSegment: null,
      traceHighlightSegIds: null,
      traceHighlightSnaps: null,
      traceLrpFocus: null,
      candidatePopup: null,
      replaySteps: [],
      replayStats: null,
      replayStep: 0,
      pinnedCandidates: {},
      forcedDecodeResult: null,
      llmMessages: [],
      llmApiHistory: [],
      llmLoading: false,
    }));
    _tileGeomCache = new Map();
    _segIdToTile   = new Map();
    _segGeomCache  = new Map();
    // Hoisted so the catch block can inspect it even if an exception occurs mid-processing.
    let result = null;
    try {
      const t0 = performance.now();
      _decoder.reset_tiles();
      const paramsJson = JSON.stringify(params);
      console.log('[params] fow_weight:', params.fow_weight, 'frc_weight:', params.frc_weight,
        'fow[3][7]:', params.fow_penalty_table[3][7], 'fow[7][3]:', params.fow_penalty_table[7][3],
        'lfrcnp_tolerance:', params.lfrcnp_tolerance);
      const startResult = JSON.parse(_decoder.start(openlrString.trim(), paramsJson, _zoom));
      console.log(`[timing] start(): ${(performance.now()-t0).toFixed(1)} ms`);

      console.log('[decode] requested tiles:', startResult.tiles.map(([z,x,y]) => `${z}/${x}/${y}`));
      let loadedTiles = 0;
      let wasmLoadMs = 0;
      let jsDecodeMs = 0;
      const tFetch0 = performance.now();
      await Promise.all(startResult.tiles.map(async ([z, x, y]) => {
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          const bytes = tileBytes(res);
          if (bytes) {
            const tWasm0 = performance.now();
            _decoder.load_tile(z, x, y, bytes);
            wasmLoadMs += performance.now() - tWasm0;
            loadedTiles++;
            const tileKey = `${z}/${x}/${y}`;
            const wasmCount = _decoder.tile_segment_count(z, x, y);
            const tJs0 = performance.now();
            _tileGeomCache.set(tileKey, decodeTile(bytes.buffer, z, x, y).features);
            jsDecodeMs += performance.now() - tJs0;
            console.log(`[tile] loaded ${tileKey} (${res.data.byteLength} raw → ${bytes.byteLength} bytes, ${wasmCount} segs in WASM)`);
          } else {
            console.warn(`[tile] no data for ${z}/${x}/${y} (tile not in archive)`);
          }
        } catch (e) {
          console.warn(`[tile] ${z}/${x}/${y} load failed:`, e?.message ?? e);
        }
      }));
      console.log(`[timing] tile fetch+load total: ${(performance.now()-tFetch0).toFixed(1)} ms  (WASM load_tile: ${wasmLoadMs.toFixed(1)} ms, JS decodeTile: ${jsDecodeMs.toFixed(1)} ms)`);

      const segs = _decoder.loaded_segment_count();
      console.log(`[decode] tiles requested=${startResult.tiles.length} loaded=${loadedTiles} segments=${segs}`);

      // Run decode, loading any tiles A* discovers it needs along the way.
      // Each call either returns a result (ok or error) or a { needs_tile: [z,x,y] }
      // signal.  We cap retries to prevent runaway in degenerate cases.
      //
      // Every branch that stops this loop early MUST leave `result` as a proper
      // { ok: false, error, segments: [], lrps: [] } object, never as a leftover
      // { needs_tile: [...] } blob — the latter has none of the fields the rest of
      // this function (and the UI toast) expect, so it silently surfaces as
      // "Decode failed" with no message and "undefined" segments/lrps.
      const attemptedTiles = new Set(startResult.tiles.map(([z,x,y]) => `${z}/${x}/${y}`));
      const MAX_DYNAMIC_LOADS = 20;
      let dynamicLoads = 0;
      while (true) {
        const tDecode0 = performance.now();
        result = JSON.parse(_decoder.decode());
        console.log(`[timing] decode() attempt ${dynamicLoads}: ${(performance.now()-tDecode0).toFixed(1)} ms`);
        if (!result.needs_tile) {
          console.log(
            `[decode-result] ok=${result.ok} format="${result.format ?? '(absent)'}"` +
            ` lrps=${result.lrps == null ? 'ABSENT' : result.lrps.length}` +
            ` trace=${result.trace == null ? 'ABSENT' : ('events=' + (result.trace.events?.length ?? '?'))}` +
            ` error="${result.error ?? ''}"`
          );
          break;
        }

        if (dynamicLoads >= MAX_DYNAMIC_LOADS) {
          // A* aborts its entire search and restarts from scratch on the very first
          // boundary node it finds needing an unloaded tile -- even one belonging to
          // a dead-end branch that will never end up on the real path (e.g. the far
          // shore of a body of water the straight-line heuristic can't see is
          // impassable). Many restarts, each individually bounded by
          // dnp.ub * max_path_search_factor but touching a different dead-end branch,
          // can union into a much wider tile footprint than any single run explores.
          // Lowering the search factor shrinks each run's blast radius, making it far
          // more likely one completes without hitting an unloaded boundary node at
          // all -- which is what actually surfaces trace data to diagnose the real
          // problem (e.g. an LFRCNP tolerance letting A* wander through low-class
          // roads). Hitting this cap is a symptom, not the disease.
          const msg = `Decode exceeded the maximum of ${MAX_DYNAMIC_LOADS} dynamically-loaded tiles — the route may span too large an area, or too many low-FRC roads are being explored. Try reducing the path search factor first: a smaller value often lets a decode complete (with real trace data to diagnose the actual cause) instead of repeatedly restarting the search on unrelated dead-end tiles.`;
          console.warn(`[tile] ${msg}`);
          result = clientDecodeFailure(msg);
          break;
        }

        const [z, x, y] = result.needs_tile;
        const tileKey = `${z}/${x}/${y}`;

        if (attemptedTiles.has(tileKey)) {
          // Guard: same tile requested twice means the graph didn't register it as
          // loaded (shouldn't happen, but prevents an infinite loop).
          const msg = `A* re-requested tile ${tileKey} that was already loaded — this indicates an internal bug`;
          console.warn(`[tile] ${msg}`);
          result = clientDecodeFailure(msg);
          break;
        }
        attemptedTiles.add(tileKey);
        dynamicLoads++;
        console.log(`[tile] A* needs ${tileKey} (dynamic load, attempt ${dynamicLoads})`);

        try {
          const res = await _pmtiles.getZxy(z, x, y);
          const bytes = tileBytes(res);
          if (bytes) {
            _decoder.load_tile(z, x, y, bytes);
            _tileGeomCache.set(tileKey, decodeTile(bytes.buffer, z, x, y).features);
            console.log(`[tile] dynamic loaded ${tileKey} (${res.data.byteLength} raw → ${bytes.byteLength} bytes)`);
          } else {
            _decoder.load_tile(z, x, y, new Uint8Array(0));
            console.warn(`[tile] dynamic ${tileKey}: not in archive, marked empty`);
          }
        } catch (e) {
          const msg = `Failed to load tile ${tileKey}: ${e?.message ?? e}`;
          console.warn(`[tile] ${msg}`);
          result = clientDecodeFailure(msg);
          break;
        }
      }

      // Build segment_id → tile + segment_id → feature maps.
      // Done after the dynamic-tile loop so all loaded tiles are included.
      // Pre-index each tile's features by local_index so the per-segment lookup is O(1)
      // rather than O(tile_size) — avoiding an O(N²) scan over 200k+ segments.
      const tIdx0 = performance.now();
      const tileFeatureIndex = new Map();
      for (const [tileKey, features] of _tileGeomCache) {
        const idx = new Map();
        for (const feat of features) idx.set(feat.properties.local_index, feat);
        tileFeatureIndex.set(tileKey, idx);
      }
      console.log(`[timing] tile feature index build: ${(performance.now()-tIdx0).toFixed(1)} ms`);

      const tMap0 = performance.now();
      const rawMappings = JSON.parse(_decoder.all_segment_tile_mappings());
      console.log(`[timing] all_segment_tile_mappings serialize+parse: ${(performance.now()-tMap0).toFixed(1)} ms`);

      const tCache0 = performance.now();
      for (const [segId, z, x, y, li] of rawMappings) {
        const tileKey = `${z}/${x}/${y}`;
        _segIdToTile.set(segId, { tile_key: tileKey, local_index: li });
        // O(1) lookup via pre-built index — was O(tile_size) with .find()
        const feat = tileFeatureIndex.get(tileKey)?.get(li);
        if (feat) _segGeomCache.set(segId, feat);
      }
      console.log(`[timing] segGeomCache build (${rawMappings.length} segs): ${(performance.now()-tCache0).toFixed(1)} ms`);
      console.log(`[segGeomCache] ${_segGeomCache.size}/${rawMappings.length} segments have geometry`);
      // Enrich decoded segments with stable_id from the tile geometry cache.
      for (const seg of result.segments ?? []) {
        const feat = _segGeomCache.get(seg.segment_id);
        if (feat) seg.stable_id = feat.properties.stable_id ?? null;
      }
      console.log('[PATH] segments:', result.segments?.map(s => s.stable_id));
      console.log('[LRPs]', result.lrps?.map((l, i) =>
        `LRP${i}: lon=${l.lon.toFixed(5)} lat=${l.lat.toFixed(5)}` +
        ` bear=[${l.bearing_lb.toFixed(2)},${l.bearing_ub.toFixed(2)}]` +
        ` frc=${l.frc} fow=${l.fow}` +
        (l.lfrcnp != null ? ` lfrcnp=${l.lfrcnp} (effective floor=${Math.min(l.lfrcnp + (params.lfrcnp_tolerance ?? 0), 7)})` : ' [last LRP]')
      ));
      if (result.trace?.events) {
        result.trace.events.filter(e => e.CandidatesRanked).forEach(e => {
          const r = e.CandidatesRanked;
          console.log(`[TRACE] LRP${r.lrp_idx} candidates (${r.accepted.length} accepted, ${r.rejected_count} rejected):`);
          r.accepted.forEach((c, i) => console.log(
            `  #${i} seg=${c.segment_id} ${c.traversal} arc=${c.projection.arc_offset_m.toFixed(1)}m` +
            ` dist=${c.projection.distance_m.toFixed(2)}m bear=${c.projection.bearing_deg.toFixed(1)}°` +
            ` score=${c.score.total.toFixed(4)} (dist=${c.score.distance_score.toFixed(4)}` +
            ` bear=${c.score.bearing_score.toFixed(4)} frc=${c.score.frc_score.toFixed(4)}` +
            ` fow=${c.score.fow_score.toFixed(4)} wrong_ep=${c.score.wrong_endpoint_score.toFixed(4)}` +
            ` int=${c.score.interior_score.toFixed(4)})`
          ));
        });
        const routes = result.trace.events.filter(e => e.RouteSearchStarted || e.DnpChecked);
        console.log('[TRACE] Routing events:', JSON.stringify(routes, null, 2));
        // Show A* termination stats — these confirm whether LFRCNP is biting
        result.trace.events.filter(e => e.AStarTerminated).forEach(e => {
          const t = e.AStarTerminated;
          console.log(
            `[TRACE] A* leg ${t.leg}: ${t.nodes_expanded} expansions, reason=${JSON.stringify(t.reason)}` +
            ` skipped: frc=${t.edges_skipped_frc} dir=${t.edges_skipped_direction}` +
            ` turn=${t.edges_skipped_turn} dist=${t.edges_skipped_distance}` +
            ` sharp_turn=${t.edges_skipped_sharp_turn}`
          );
        });
      }
      // Build replay steps from trace events (if any)
      const replayData = result.trace?.events?.length
        ? buildReplaySteps(result.trace.events)
        : { steps: [], stats: null };
      const toast = result.ok ? null : { message: result.error ?? 'Decode failed' };
      set({
        decoding: false,
        decodeResult: result,
        decodeToast: toast,
        replaySteps: replayData.steps,
        replayStats:  replayData.stats,
        replayStep:   0,
      });
    } catch (e) {
      const stage = result !== null ? 'post-decode JS' : 'pre-decode (start/tile-load)';
      console.error(`[decode] exception in runDecode at ${stage}:`, e);
      console.error('[decode] result at throw time:', result);
      // result.ok is a boolean iff WASM returned a proper DecodeResult.  Preserve it — it
      // carries lrps/trace we want to show.  The exception came from post-decode JS processing.
      if (result !== null && (result.ok === true || result.ok === false)) {
        const toast = result.ok ? null : { message: result.error ?? 'Decode failed' };
        set({ decoding: false, decodeResult: result, decodeToast: toast, replaySteps: [], replayStats: null, replayStep: 0 });
      } else {
        // WASM throws plain strings via JsValue::from_str; JS Error objects have .message.
        const errorMsg = e instanceof Error ? e.message : String(e);
        set({ decoding: false, decodeResult: clientDecodeFailure(errorMsg), decodeToast: { message: errorMsg } });
      }
    }
  },

  // ── Encode mode ───────────────────────────────────────────────────────────

  setMode: (mode) => set({ mode }),
  // Switching to a *different* location type invalidates whatever's already
  // drawn for the old one (e.g. a multi-waypoint Line route makes no sense
  // once switched to PointAlongLine's single point) — clear all encode
  // artifacts rather than leave stale, mismatched-type state on the map.
  // Not pushed onto waypointHistory: undoing back into a route built for a
  // different location type wouldn't make sense either, so the old history
  // is dropped along with the waypoints themselves.
  setLocationType: (locationType) => set(state => {
    if (locationType === state.locationType) return { locationType };
    return {
      locationType,
      waypoints: [],
      waypointHistory: [],
      liveRoute: null,
      liveRouteError: null,
      encodeResult: null,
      verifyResult: null,
      verifyToast: null,
      verifyReplaySteps: [],
      verifyReplayStats: null,
      verifyReplayStep: 0,
    };
  }),
  setPalOrientation: (palOrientation) => set({ palOrientation }),
  setPalSideOfRoad:  (palSideOfRoad) => set({ palSideOfRoad }),
  setMaxEncodeLegM: (maxEncodeLegM) => set({ maxEncodeLegM }),

  // Bulk-replace all waypoints at once (e.g. from the debug textarea input
  // before map-based drawing exists). Prefetches tiles for every point.
  setWaypoints: async (list) => {
    const { waypoints, locationType } = get();
    // PointAlongLine has exactly one point — pasting a multi-line list only
    // ever keeps the first, matching addWaypoint's replace-not-append rule.
    const next = locationType === 'PointAlongLine' ? list.slice(0, 1) : list;
    set(state => ({ waypointHistory: [...state.waypointHistory, waypoints], waypoints: next }));
    await Promise.all(next.map(w => loadEncoderTilesNear(w.lon, w.lat)));
    await get().runLiveRoute();
  },

  addWaypoint: async (lonLat) => {
    const { waypoints, locationType } = get();
    // PointAlongLine has exactly one point, never a waypoint list — placing
    // a new one replaces it rather than appending, so there's never an
    // ambiguous "only the first is used" list to reason about.
    const next = locationType === 'PointAlongLine' ? [lonLat] : [...waypoints, lonLat];
    set(state => ({ waypointHistory: [...state.waypointHistory, waypoints], waypoints: next }));
    await loadEncoderTilesNear(lonLat.lon, lonLat.lat);
    await get().runLiveRoute();
  },

  insertWaypoint: async (index, lonLat) => {
    const { waypoints } = get();
    const next = [...waypoints.slice(0, index), lonLat, ...waypoints.slice(index)];
    set(state => ({ waypointHistory: [...state.waypointHistory, waypoints], waypoints: next }));
    await loadEncoderTilesNear(lonLat.lon, lonLat.lat);
    await get().runLiveRoute();
  },

  moveWaypoint: async (index, lonLat) => {
    const { waypoints } = get();
    const next = waypoints.map((w, i) => (i === index ? lonLat : w));
    set(state => ({ waypointHistory: [...state.waypointHistory, waypoints], waypoints: next }));
    await loadEncoderTilesNear(lonLat.lon, lonLat.lat);
    await get().runLiveRoute();
  },

  removeWaypoint: async (index) => {
    const { waypoints } = get();
    const next = waypoints.filter((_, i) => i !== index);
    set(state => ({ waypointHistory: [...state.waypointHistory, waypoints], waypoints: next }));
    await get().runLiveRoute();
  },

  // Swap waypoint `index` with its neighbor in `direction` (-1 or +1) —
  // reordering via the waypoint list in the Results panel's encode view.
  moveWaypointIndex: async (index, direction) => {
    const { waypoints } = get();
    const j = index + direction;
    if (j < 0 || j >= waypoints.length) return;
    const next = [...waypoints];
    [next[index], next[j]] = [next[j], next[index]];
    set(state => ({ waypointHistory: [...state.waypointHistory, waypoints], waypoints: next }));
    await get().runLiveRoute();
  },

  undo: async () => {
    const { waypointHistory } = get();
    if (!waypointHistory.length) return;
    const prev = waypointHistory[waypointHistory.length - 1];
    set({ waypoints: prev, waypointHistory: waypointHistory.slice(0, -1) });
    await get().runLiveRoute();
  },

  clearWaypoints: () => {
    const { waypoints } = get();
    set(state => ({
      waypointHistory: [...state.waypointHistory, waypoints],
      waypoints: [],
      liveRoute: null,
      liveRouteError: null,
      encodeResult: null,
      verifyResult: null,
      verifyToast: null,
      verifyReplaySteps: [],
      verifyReplayStats: null,
      verifyReplayStep: 0,
      // Clearing is a decisive "start over" — with nothing left to review or
      // encode, the panel would just show empty-state placeholders; closing
      // it returns to the same collapsed posture the encode flow starts in.
      showResult: false,
    }));
  },

  // Layer 1: snap waypoints to the road network and chain shortest-path
  // between consecutive points, loading any tile the search discovers it
  // needs along the way. Mirrors runDecode's needs_tile retry loop.
  runLiveRoute: async () => {
    const { waypoints, params } = get();
    if (waypoints.length < 2 || !_encoder || !_pmtiles) {
      set({ liveRoute: null, liveRouteError: null });
      return;
    }
    set({ liveRouteLoading: true });
    try {
      const attemptedTiles = new Set();
      const MAX_DYNAMIC_LOADS = 20;
      let dynamicLoads = 0;
      let out = null;
      while (true) {
        out = JSON.parse(_encoder.route_between(JSON.stringify(waypoints), params.max_interior_turn_deviation_deg, _zoom));
        if (!out.needs_tile) break;
        if (dynamicLoads >= MAX_DYNAMIC_LOADS) {
          out = { error: `Route search exceeded the maximum of ${MAX_DYNAMIC_LOADS} dynamically-loaded tiles.` };
          break;
        }
        const [z, x, y] = out.needs_tile;
        const tileKey = `${z}/${x}/${y}`;
        if (attemptedTiles.has(tileKey)) {
          out = { error: `Route search re-requested tile ${tileKey} that was already loaded — internal bug` };
          break;
        }
        attemptedTiles.add(tileKey);
        dynamicLoads++;
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          const bytes = tileBytes(res);
          _encoder.load_tile(z, x, y, bytes ?? new Uint8Array(0));
        } catch (e) {
          out = { error: `Failed to load tile ${tileKey}: ${e?.message ?? e}` };
          break;
        }
      }
      if (out.error) {
        set({ liveRoute: null, liveRouteError: out.error, liveRouteLoading: false });
      } else {
        set({ liveRoute: out, liveRouteError: null, liveRouteLoading: false });
      }
    } catch (e) {
      set({ liveRoute: null, liveRouteError: String(e), liveRouteLoading: false });
    }
  },

  // Encode the current waypoints to a Line location (v3 + TPEG), then
  // immediately decode the v3 string back via the existing _decoder so
  // Results/Trace/Replay can show a real round-trip verification.
  runEncode: async () => {
    const { waypoints, params, maxEncodeLegM } = get();
    if (waypoints.length < 2 || !_encoder || !_pmtiles) return;
    set({
      encoding: true, encodeResult: null,
      verifyResult: null, verifyToast: null, verifyReplaySteps: [], verifyReplayStats: null, verifyReplayStep: 0,
    });
    try {
      const attemptedTiles = new Set();
      const MAX_DYNAMIC_LOADS = 20;
      let dynamicLoads = 0;
      let out = null;
      // max_interior_turn_deviation_deg applies to encoding too, despite the
      // decode-only-sounding name — see openlr-encoder::coverage::sweep_coverage.
      while (true) {
        out = JSON.parse(_encoder.encode_line(JSON.stringify(waypoints), params.max_interior_turn_deviation_deg, maxEncodeLegM, _zoom));
        if (!out.needs_tile) break;
        if (dynamicLoads >= MAX_DYNAMIC_LOADS) {
          out = { error: `Encode exceeded the maximum of ${MAX_DYNAMIC_LOADS} dynamically-loaded tiles.` };
          break;
        }
        const [z, x, y] = out.needs_tile;
        const tileKey = `${z}/${x}/${y}`;
        if (attemptedTiles.has(tileKey)) {
          out = { error: `Encode re-requested tile ${tileKey} that was already loaded — internal bug` };
          break;
        }
        attemptedTiles.add(tileKey);
        dynamicLoads++;
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          const bytes = tileBytes(res);
          _encoder.load_tile(z, x, y, bytes ?? new Uint8Array(0));
        } catch (e) {
          out = { error: `Failed to load tile ${tileKey}: ${e?.message ?? e}` };
          break;
        }
      }

      if (out.error) {
        set({ encoding: false, encodeResult: encodeErrorResult(out) });
        return;
      }

      set({ encodeResult: { v3: out.v3, tpeg: out.tpeg, error: null } });
      await get().runVerifyDecode(out.v3);
    } catch (e) {
      set({ encodeResult: { v3: null, tpeg: null, error: String(e) } });
    } finally {
      set({ encoding: false });
    }
  },

  // Encode the first waypoint as a PointAlongLine location, then verify via
  // round-trip decode exactly like runEncode does for Line. Orientation/
  // side-of-road are read from the store (not passed in) so the map's
  // right-click popup and the Results panel's selects share one source of
  // truth regardless of which one was touched last.
  runEncodePal: async () => {
    const { waypoints, params, maxEncodeLegM, palOrientation: orientation, palSideOfRoad: sideOfRoad } = get();
    if (waypoints.length < 1 || !_encoder) return;
    set({
      encoding: true, encodeResult: null,
      verifyResult: null, verifyToast: null, verifyReplaySteps: [], verifyReplayStats: null, verifyReplayStep: 0,
    });
    try {
      const { lon, lat, segment_id, node_id } = waypoints[0];
      const out = JSON.parse(_encoder.encode_pal(lon, lat, segment_id, node_id, orientation, sideOfRoad, params.max_interior_turn_deviation_deg, maxEncodeLegM));
      if (out.error) {
        set({ encodeResult: encodeErrorResult(out) });
        return;
      }
      set({ encodeResult: { v3: out.v3, tpeg: out.tpeg, error: null } });
      await get().runVerifyDecode(out.v3);
    } catch (e) {
      set({ encodeResult: { v3: null, tpeg: null, error: String(e) } });
    } finally {
      set({ encoding: false });
    }
  },

  // Decode `v3String` with the existing _decoder, following the same
  // needs_tile retry protocol and segment-geometry cache build-out as
  // runDecode, but writing to verifyResult/verifyReplay* instead.
  runVerifyDecode: async (v3String) => {
    const { params } = get();
    if (!v3String || !_pmtiles || !_decoder) return;
    let result = null;
    try {
      _decoder.reset_tiles();
      const paramsJson = JSON.stringify(params);
      const startResult = JSON.parse(_decoder.start(v3String, paramsJson, _zoom));

      await Promise.all(startResult.tiles.map(async ([z, x, y]) => {
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          const bytes = tileBytes(res);
          if (bytes) {
            _decoder.load_tile(z, x, y, bytes);
            _tileGeomCache.set(`${z}/${x}/${y}`, decodeTile(bytes.buffer, z, x, y).features);
          }
        } catch (e) {
          console.warn(`[verify-decode] tile ${z}/${x}/${y} load failed:`, e?.message ?? e);
        }
      }));

      const attemptedTiles = new Set(startResult.tiles.map(([z,x,y]) => `${z}/${x}/${y}`));
      const MAX_DYNAMIC_LOADS = 20;
      let dynamicLoads = 0;
      while (true) {
        result = JSON.parse(_decoder.decode());
        if (!result.needs_tile) break;
        if (dynamicLoads >= MAX_DYNAMIC_LOADS) {
          result = clientDecodeFailure(`Verify-decode exceeded the maximum of ${MAX_DYNAMIC_LOADS} dynamically-loaded tiles.`);
          break;
        }
        const [z, x, y] = result.needs_tile;
        const tileKey = `${z}/${x}/${y}`;
        if (attemptedTiles.has(tileKey)) {
          result = clientDecodeFailure(`Verify-decode re-requested tile ${tileKey} that was already loaded — internal bug`);
          break;
        }
        attemptedTiles.add(tileKey);
        dynamicLoads++;
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          const bytes = tileBytes(res);
          if (bytes) {
            _decoder.load_tile(z, x, y, bytes);
            _tileGeomCache.set(tileKey, decodeTile(bytes.buffer, z, x, y).features);
          } else {
            _decoder.load_tile(z, x, y, new Uint8Array(0));
          }
        } catch (e) {
          result = clientDecodeFailure(`Failed to load tile ${tileKey}: ${e?.message ?? e}`);
          break;
        }
      }

      // Rebuild segment_id → tile/geometry caches so Results/Segments can
      // draw the verified route, same as runDecode's post-processing.
      const tileFeatureIndex = new Map();
      for (const [tileKey, features] of _tileGeomCache) {
        const idx = new Map();
        for (const feat of features) idx.set(feat.properties.local_index, feat);
        tileFeatureIndex.set(tileKey, idx);
      }
      const rawMappings = JSON.parse(_decoder.all_segment_tile_mappings());
      for (const [segId, z, x, y, li] of rawMappings) {
        const tileKey = `${z}/${x}/${y}`;
        _segIdToTile.set(segId, { tile_key: tileKey, local_index: li });
        const feat = tileFeatureIndex.get(tileKey)?.get(li);
        if (feat) _segGeomCache.set(segId, feat);
      }
      for (const seg of result.segments ?? []) {
        const feat = _segGeomCache.get(seg.segment_id);
        if (feat) seg.stable_id = feat.properties.stable_id ?? null;
      }

      const replayData = result.trace?.events?.length
        ? buildReplaySteps(result.trace.events)
        : { steps: [], stats: null };
      set({
        verifyResult: result,
        verifyToast: result.ok ? null : { message: result.error ?? 'Verify-decode failed' },
        verifyReplaySteps: replayData.steps,
        verifyReplayStats: replayData.stats,
        verifyReplayStep: 0,
      });
    } catch (e) {
      const errorMsg = e instanceof Error ? e.message : String(e);
      set({ verifyResult: clientDecodeFailure(errorMsg), verifyToast: { message: errorMsg } });
    }
  },
 }),
 {
   name: 'openlrlens-settings',
   partialize: (state) => ({
     openlrString: state.openlrString,
     tileUrl: state.tileUrl,
     params: state.params,
     savedParamSets: state.savedParamSets,
     maxEncodeLegM: state.maxEncodeLegM,
   }),
   // Deep-merge params so new fields added to PRESETS.Default survive across
   // localStorage upgrades — persisted values win, but missing fields fall back
   // to the current default rather than becoming undefined.
   merge: (persisted, current) => ({
     ...current,
     ...persisted,
     params: { ...current.params, ...persisted.params },
     savedParamSets: { ...(persisted.savedParamSets ?? {}) },
   }),
 }
));
