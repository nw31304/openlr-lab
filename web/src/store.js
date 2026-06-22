import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import { decodeTile } from './tileDecoder.js';
import { buildReplaySteps } from './replayEngine.js';
import { loadLlmConfig, saveLlmConfig, clearLlmConfig as clearLlmStorage, chatComplete } from './llmClient.js';
import { buildSystemContext } from './llmDiagnosis.js';

let _pmtiles = null;
let _decoder = null;
let _zoom = 12;
/** segment_id → { tile_key, local_index } — rebuilt after every decode */
let _segIdToTile = new Map();
/** tile_key → GeoJSON features[] — built from tile bytes during decode */
let _tileGeomCache = new Map();
/** segment_id → GeoJSON feature — direct lookup, built from the two caches above */
let _segGeomCache = new Map();

export function setPmtiles(p) { _pmtiles = p; }
export function setDecoder(d) { _decoder = d; }
export function setZoom(z)    { _zoom = z; }
export function getSegIdToTile()   { return _segIdToTile; }
export function getTileGeomCache() { return _tileGeomCache; }
export function getSegGeomCache()  { return _segGeomCache; }

/** Look up the internal graph segment ID by tile + local index.  Returns -1 when
 *  the tile hasn't been loaded by the decoder yet (e.g. before the first decode). */
export function getSegmentId(z, x, y, localIndex) {
  if (!_decoder) return -1;
  return _decoder.segment_id_at(z, x, y, localIndex);
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
    trace_level: 'Summary',
  },
};

export const useStore = create(persist(
 (set, get) => ({
  openlrString: '',
  tileUrl: 'http://localhost:5176',
  params: { ...PRESETS.Default },
  showParams: false,
  showLlmSettings: false,
  showTrace: false,
  showResult: false,
  llmConfig: loadLlmConfig(),
  llmChatOpen: false,
  llmMessages: [],   // { role: 'user'|'assistant', content: string, error?: bool }
  llmLoading: false,
  showSegmentLayer: false,
  decoding: false,
  decodeResult: null,
  highlightedSegment: null,
  traceHighlightSegIds: null,
  traceLrpFocus: null,
  candidatePopup: null,
  // ── Replay state ─────────────────────────────────────────────────────────
  replaySteps: [],        // pre-built display steps from buildReplaySteps()
  replayStats: null,      // { maxG, totalNodes, phases }
  replayStep: 0,          // current display step index

  setOpenlrString: (s) => set({ openlrString: s }),
  setTileUrl: (url) => set({ tileUrl: url }),

  resetToDefaults: () => set({ params: { ...PRESETS.Default } }),

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
  clearLlmChat:  () => set({ llmMessages: [], llmLoading: false }),

  // content = text sent to the API (may include appended format hints)
  // display = text shown in the chat bubble (the user's original words)
  sendLlmMessage: async (content, display) => {
    const { llmMessages, decodeResult, params, llmConfig } = get();
    if (!llmConfig || !decodeResult) return;
    const userMsg = { role: 'user', content, display: display ?? content };
    set({ llmMessages: [...llmMessages, userMsg], llmLoading: true });
    const systemContext = buildSystemContext(decodeResult, params);
    const history = [
      { role: 'system', content: systemContext },
      ...llmMessages.map(m => ({ role: m.role, content: m.content })),
      { role: 'user', content },
    ];
    const result = await chatComplete(llmConfig, history);
    const assistantMsg = result.ok
      ? { role: 'assistant', content: result.content ?? '' }
      : { role: 'assistant', content: result.error ?? 'Unknown error', error: true };
    set(s => ({ llmMessages: [...s.llmMessages, assistantMsg], llmLoading: false }));
  },
  toggleTrace:         () => set(state => ({ showTrace:         !state.showTrace })),
  toggleSegmentLayer:  () => set(state => ({ showSegmentLayer:  !state.showSegmentLayer })),

  setReplayStep:  (n)  => set(state => ({ replayStep: Math.max(0, Math.min(n, state.replaySteps.length - 1)) })),
  stepReplay: (delta) => set(state => ({
    replayStep: Math.max(0, Math.min(state.replayStep + delta, state.replaySteps.length - 1)),
  })),

  // Re-decode at an elevated trace level and open the trace panel.
  // Off → Summary on first call; Summary or Full → Full on subsequent calls.
  debugDecode: async () => {
    const { params } = get();
    const current = params.trace_level ?? 'Summary';
    const elevated = current === 'Off' ? 'Summary' : 'Full';
    set(state => ({ params: { ...state.params, trace_level: elevated }, showTrace: true }));
    await get().runDecode();
  },

  hideResult:    () => set({ showResult: false }),
  toggleResult:  () => set(state => ({ showResult: !state.showResult })),
  clearResult: () => set({ decodeResult: null, showResult: false, highlightedSegment: null, traceHighlightSegIds: null, traceLrpFocus: null, candidatePopup: null }),
  setHighlightedSegment: (seg) => set({ highlightedSegment: seg }),
  // Request the segment info popup to open for a given tile+local_index.
  // Map.jsx watches this and opens the popup; call clearRequestedInfoSegment() after handling.
  requestedInfoSegment: null,
  requestInfoSegment:      (tile, local_index) => set({ requestedInfoSegment: { tile, local_index } }),
  clearRequestedInfoSegment: () => set({ requestedInfoSegment: null }),
  setTraceHighlight: (ids) => set({ traceHighlightSegIds: ids?.length ? ids : null }),
  setCandidatePopup: (data) => set({ candidatePopup: data }),
  clearCandidatePopup: () => set({ candidatePopup: null }),
  setTraceLrpFocus: (lrp) => set({ traceLrpFocus: lrp ? { ...lrp, _tick: Date.now() } : null }),

  runDecode: async () => {
    const { openlrString, params } = get();
    if (!openlrString.trim() || !_pmtiles || !_decoder) return;

    set({ decoding: true, decodeResult: null, highlightedSegment: null, traceHighlightSegIds: null, candidatePopup: null, llmMessages: [], llmLoading: false });
    _tileGeomCache = new Map();
    _segIdToTile   = new Map();
    _segGeomCache  = new Map();
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
          if (res?.data) {
            const tWasm0 = performance.now();
            _decoder.load_tile(z, x, y, new Uint8Array(res.data));
            wasmLoadMs += performance.now() - tWasm0;
            loadedTiles++;
            const tileKey = `${z}/${x}/${y}`;
            const wasmCount = _decoder.tile_segment_count(z, x, y);
            const tJs0 = performance.now();
            // Cache tile geometry so the trace panel can pan/highlight decoded segments
            _tileGeomCache.set(tileKey, decodeTile(res.data, z, x, y).features);
            jsDecodeMs += performance.now() - tJs0;
            console.log(`[tile] loaded ${tileKey} (${res.data.byteLength} bytes, ${wasmCount} segs in WASM)`);
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

      // Build segment_id → tile + segment_id → feature maps.
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

      const tDecode0 = performance.now();
      const result = JSON.parse(_decoder.decode());
      console.log(`[timing] decode() WASM: ${(performance.now()-tDecode0).toFixed(1)} ms`);
      // Enrich decoded segments with source_id from the tile geometry cache.
      for (const seg of result.segments ?? []) {
        const feat = _segGeomCache.get(seg.segment_id);
        if (feat) seg.source_id = feat.properties.source_id ?? null;
      }
      console.log('[PATH] segments:', result.segments?.map(s => s.source_id));
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
            ` turn=${t.edges_skipped_turn} dist=${t.edges_skipped_distance}`
          );
        });
      }
      // Build replay steps from trace events (if any)
      const replayData = result.trace?.events?.length
        ? buildReplaySteps(result.trace.events)
        : { steps: [], stats: null };
      set({
        decoding: false,
        decodeResult: result,
        showResult: true,
        replaySteps: replayData.steps,
        replayStats:  replayData.stats,
        replayStep:   0,
      });
    } catch (e) {
      set({ decoding: false, decodeResult: { ok: false, error: e.message, segments: [] } });
    }
  },
 }),
 {
   name: 'openlrlens-settings',
   partialize: (state) => ({
     openlrString: state.openlrString,
     tileUrl: state.tileUrl,
     params: state.params,
   }),
   // Deep-merge params so new fields added to PRESETS.Default survive across
   // localStorage upgrades — persisted values win, but missing fields fall back
   // to the current default rather than becoming undefined.
   merge: (persisted, current) => ({
     ...current,
     ...persisted,
     params: { ...current.params, ...persisted.params },
   }),
 }
));
