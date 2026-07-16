// Tool definitions (OpenAI function-calling format) and executor.
// llmClient.js converts these to Anthropic format when needed.

const FOW_LABELS = [
  'undefined', 'motorway', 'dual carriageway', 'single carriageway',
  'roundabout', 'traffic square', 'slip road', 'other',
];
const FRC_LABELS = [
  'FRC0 motorway', 'FRC1 trunk', 'FRC2 secondary', 'FRC3 tertiary',
  'FRC4 unclassified', 'FRC5 residential', 'FRC6 service', 'FRC7 other',
];

const FOW_LABELS_FULL = [
  'Form of Way undefined', 'Motorway', 'Multiple carriageway', 'Single carriageway',
  'Roundabout', 'Traffic square', 'Slip road', 'Other / non-vehicle',
];

export const TOOL_DEFINITIONS = [
  {
    type: 'function',
    function: {
      name: 'get_decode_summary',
      description:
        'Top-level decode outcome: success/failure, segment count, format, offset ranges, current decode parameters, and the full ordered path segment list with per-segment length_m, FRC, FOW, and direction. Also returns path_total_length_m — the sum of all segment lengths in the untrimmed decoded location. Call this first before any other tool.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_parsed_reference',
      description:
        'Full parsed LRP chain: coordinates, bearing interval, FRC, FOW, LFRCNP, and DNP for each LRP.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_lrp_candidates',
      description:
        'Ranked candidate segments for one LRP, with projection geometry and score breakdown. Set include_rejected=true to see rejection reasons.',
      parameters: {
        type: 'object',
        properties: {
          lrp_index: {
            type: 'integer',
            description: '1-based LRP index (LRP 1 = first, LRP 2 = second, etc.).',
          },
          include_rejected: {
            type: 'boolean',
            description: 'Include rejected candidates with their rejection verdict. Default false.',
          },
        },
        required: ['lrp_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_leg_summary',
      description:
        'Routing outcome for one inter-LRP leg: whether a route was found, its length, A* expansion statistics (nodes expanded, edges skipped by reason), and DNP validation result.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: {
            type: 'integer',
            description: '1-based leg index (leg 1 = LRP 1 → LRP 2).',
          },
        },
        required: ['leg_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_route_segments',
      description:
        'Ordered segment list for a successfully routed leg, with per-segment length_m, FRC, FOW, direction, and cumulative distance. Also returns segment_sum_m and snap coordinates at each end.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: {
            type: 'integer',
            description: '1-based leg index.',
          },
        },
        required: ['leg_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_segment',
      description:
        'Full attributes and geometry for one segment by its internal segment ID. Returns FRC, FOW, direction, length, geometry, tile location, and stable_id (the human-readable stable ID such as "372358612-1"). Use this to inspect any segment seen in candidate lists, path breakdowns, or rejection reasons.',
      parameters: {
        type: 'object',
        properties: {
          segment_id: {
            type: 'integer',
            description: 'Internal graph segment ID (as seen in candidate or path data).',
          },
        },
        required: ['segment_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_segments_near',
      description:
        'Find all loaded road segments within radius_m of a coordinate. Returns up to 20 segments sorted by distance, each with stable_id (stable ID like "372358612-1"), FRC, FOW, direction, and length. Useful for understanding what roads are available near an LRP that produced no or few candidates.',
      parameters: {
        type: 'object',
        properties: {
          lat:      { type: 'number',  description: 'Latitude in decimal degrees.' },
          lon:      { type: 'number',  description: 'Longitude in decimal degrees.' },
          radius_m: { type: 'number',  description: 'Search radius in metres (max 500). Default 100.' },
        },
        required: ['lat', 'lon'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_segment_neighbors',
      description:
        'Returns all segments connected at each endpoint of a given segment. '
        + 'Reports two groups — at_start_node and at_end_node — each listing every other '
        + 'segment that shares that node, with can_arrive/can_depart flags and turn-restriction flags. '
        + 'For bidirectional (Both) segments each endpoint is simultaneously entry and exit, '
        + 'so both groups show full connectivity. '
        + 'Each neighbour includes stable_id (the human-readable stable ID such as "372358612-1"), '
        + 'internal segment_id, FRC, FOW, direction, and length. '
        + 'Use this to understand junction topology, diagnose why A* took or avoided a turn, '
        + 'or explore the road network around a candidate segment.',
      parameters: {
        type: 'object',
        properties: {
          segment_id: {
            type: 'integer',
            description: 'Internal graph segment ID.',
          },
        },
        required: ['segment_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'retry_decode',
      description:
        'Re-run the decode with a partial parameter override merged over the current params. Returns ok/fail, segment count, and total path length so you can immediately compare with the original result. Tiles must already be loaded (always true after a normal decode). Example: {"max_bearing_deviation_deg": 30} to test a wider bearing window.',
      parameters: {
        type: 'object',
        properties: {
          params_override: {
            type: 'object',
            description: 'Partial DecodeParams as a JSON object — only the fields you want to change. All other params inherit from the current values.',
            additionalProperties: true,
          },
        },
        required: ['params_override'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'highlight_segments',
      description:
        'Highlight one or more road segments on the map immediately. '
        + 'Use this to visually direct the user\'s attention to specific segments you are discussing. '
        + 'Replaces any previous highlight. Pass an empty array to clear.',
      parameters: {
        type: 'object',
        properties: {
          segment_ids: {
            type: 'array',
            items: { type: 'integer' },
            description: 'Internal segment IDs to highlight (from candidate lists, path breakdowns, or get_segment).',
          },
        },
        required: ['segment_ids'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'set_map_view',
      description:
        'Pan and zoom the map to a specific coordinate. '
        + 'Use this to focus the user\'s view on the area you are discussing.',
      parameters: {
        type: 'object',
        properties: {
          lat:  { type: 'number',  description: 'Latitude in decimal degrees.' },
          lon:  { type: 'number',  description: 'Longitude in decimal degrees.' },
          zoom: { type: 'number',  description: 'Map zoom level (12–18 typical; 15 for street-level detail, 17 for junction-level).' },
        },
        required: ['lat', 'lon', 'zoom'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'focus_lrp',
      description:
        'Pan and zoom the map to an LRP coordinate at street-level zoom. '
        + 'Convenience wrapper around set_map_view — no need to look up coordinates manually.',
      parameters: {
        type: 'object',
        properties: {
          lrp_index: { type: 'integer', description: '1-based LRP index.' },
        },
        required: ['lrp_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_astar_skipped_edges',
      description:
        'List every edge skipped by A* on a specific leg, with the skip reason. '
        + 'Requires Full trace level — returns an error if only Summary trace is available. '
        + 'Optionally filter to a specific segment_id to check whether A* skipped it and why. '
        + 'Use this to diagnose why a specific road was not used despite being reachable.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: {
            type: 'integer',
            description: '1-based leg index.',
          },
          segment_id: {
            type: 'integer',
            description: 'Optional: filter to this specific segment. If omitted, returns all skipped edges.',
          },
        },
        required: ['leg_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_forced_leg_summary',
      description:
        'Routing outcome for one leg of the most recent forced decode: A* stats and DNP validation. '
        + 'Use this after run_forced_decode fails to diagnose why a specific leg did not route. '
        + 'Returns an error if no forced decode has been run yet.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: { type: 'integer', description: '1-based leg index.' },
        },
        required: ['leg_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_attempted_combinations',
      description:
        'List every candidate combination the original decode attempted, with per-combination outcome. '
        + 'Each row shows which from/to candidates were tried for each leg and whether routing succeeded. '
        + 'Use this to understand the full search space and identify which combinations were never tried.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'set_pinned_candidates',
      description:
        'Pin one specific accepted candidate per LRP so that run_forced_decode routes through exactly those snap points. '
        + 'Clears any existing pins first. Every LRP in the reference must be covered — pass one entry per LRP index. '
        + 'Snap geometry is resolved automatically from the trace; you only need segment_id and traversal from get_lrp_candidates. '
        + 'Returns the number of LRPs pinned or an error if a specified candidate was not in the accepted list.',
      parameters: {
        type: 'object',
        properties: {
          snaps: {
            type: 'array',
            description: 'One entry per LRP, in LRP order.',
            items: {
              type: 'object',
              properties: {
                lrp_index:  { type: 'integer', description: '1-based LRP index.' },
                segment_id: { type: 'integer', description: 'Internal segment ID from get_lrp_candidates.' },
                traversal:  { type: 'string',  description: '"Forward" or "Backward".' },
              },
              required: ['lrp_index', 'segment_id', 'traversal'],
            },
          },
        },
        required: ['snaps'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'run_forced_decode',
      description:
        'Run A* using the currently pinned candidates (set via set_pinned_candidates). '
        + 'Skips candidate selection entirely — only routes between the pinned snap points. '
        + 'Returns ok/fail, segment count, total path length, and per-leg DNP outcome. '
        + 'All LRPs must be pinned before calling; returns an error otherwise.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_route_geometry',
      description:
        'Return the decoded path as pre-built SVG elements (route_path, lrp_markers, scale_bar) '
        + 'for embedding in a <diagram>. The note field shows the wrapper SVG template. '
        + 'Geometry is subsampled to stay token-efficient. L0=green, last LRP=red, intermediate=orange. '
        + 'Use whenever you want to show the user a visual overview of the decoded route.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'check_path_feasibility',
      description:
        'Check whether a specific sequence of segments is traversable under the current decode constraints '
        + '(LFRCNP, direction, turn restrictions, connectivity). Returns feasible: true/false plus, when '
        + 'blocked, a per-step table with the reason (FrcBelowLfrcnp | NotConnected | WrongDirection | TurnRestriction). '
        + 'Get segment IDs from get_route_segments or get_segment_neighbors. '
        + 'Use when investigating why A* did not route via an expected road.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: {
            type: 'integer',
            description: '1-based leg index (determines the LFRCNP constraint).',
          },
          segment_ids: {
            type: 'array',
            items: { type: 'integer' },
            description: 'Ordered segment IDs representing the path to check.',
          },
        },
        required: ['leg_index', 'segment_ids'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'score_path',
      description:
        'Compute the total length of a proposed segment sequence and compare it against the DNP '
        + 'validation window for a leg. Returns proposed_length_m, actual_chosen_length_m, delta_m, '
        + 'and dnp_passes. Use after check_path_feasibility confirms feasibility — if the expected path '
        + 'is feasible but longer/shorter than the actual path, DNP tolerance may be the constraint.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: {
            type: 'integer',
            description: '1-based leg index (supplies the DNP window).',
          },
          segment_ids: {
            type: 'array',
            items: { type: 'integer' },
            description: 'Ordered segment IDs representing the path to score.',
          },
        },
        required: ['leg_index', 'segment_ids'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_junction_topology',
      description:
        'Return all segments meeting at a specific node with FRC, FOW, direction, can_arrive/can_depart, '
        + 'and turn-restriction flags. Get node_id from get_segment (start_node or end_node fields). '
        + 'Optionally pass hint_segment_id (any segment known to touch this node) to skip the path scan. '
        + 'Use when investigating why A* turned or failed to turn at a specific junction.',
      parameters: {
        type: 'object',
        properties: {
          node_id: {
            type: 'integer',
            description: 'Internal node ID (from get_segment start_node or end_node).',
          },
          hint_segment_id: {
            type: 'integer',
            description: 'Optional: any segment that touches this node, for faster lookup.',
          },
        },
        required: ['node_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_bearing_geometry',
      description:
        'Full bearing analysis for one candidate at one LRP: computed bearing_deg, encoded interval, '
        + 'effective interval after tolerance, pass/fail verdict, excess_deg when failing, snap coordinates, '
        + 'and segment geometry trimmed to ±60 m around the snap point. '
        + 'Works for both accepted and rejected candidates. '
        + 'Get segment_id from get_lrp_candidates (pass include_rejected: true for rejected candidates). '
        + 'Use to produce a bearing-wedge diagram or explain a bearing rejection.',
      parameters: {
        type: 'object',
        properties: {
          lrp_index: {
            type: 'integer',
            description: '1-based LRP index.',
          },
          segment_id: {
            type: 'integer',
            description: 'Internal segment ID of the candidate to inspect.',
          },
        },
        required: ['lrp_index', 'segment_id'],
      },
    },
  },
];

// ── Encode-side tool definitions ──────────────────────────────────────────────
//
// Encoding has no passive trace log the way decode does — sweep_coverage and
// route_waypoints run once, synchronously, with no per-decision event log to
// hand the model. These tools are on-demand diagnostics instead: they re-run
// a small, targeted piece of the graph search (a connectivity probe, a
// boundary-expansion replay, a single turn-angle check) only when the model
// asks, spending an extra A*/walk internally to turn an opaque "no route"
// string into a precise structured answer. See llm/SYSTEM_PROMPT.md's
// "Encoding" section for when to reach for which one.
//
// `highlight_segments`/`set_map_view` from TOOL_DEFINITIONS above are reused
// as-is here (same objects) — map control has no decode/encode distinction.
const highlightSegmentsTool = TOOL_DEFINITIONS.find(t => t.function.name === 'highlight_segments');
const setMapViewTool = TOOL_DEFINITIONS.find(t => t.function.name === 'set_map_view');

export const ENCODE_TOOL_DEFINITIONS = [
  {
    type: 'function',
    function: {
      name: 'get_encode_summary',
      description:
        'Top-level encode outcome: success/failure, waypoint count, live-route preview stats, active '
        + 'max_leg_m (Rule-1 cap) and turn-angle cap, and — on failure — the structured from_node/to_node/'
        + 'from_segment_id of the specific waypoint-to-waypoint leg that failed (when available; feed these '
        + 'straight into diagnose_waypoint_connection). Also reports whether a round-trip verify decode ran '
        + 'and whether it passed. Call this first before any other encode tool.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_waypoints',
      description: 'The ordered list of user-placed waypoints (lon, lat) that were fed to the encoder.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_encode_segment',
      description:
        'Full attributes and geometry for one segment by internal segment ID, from the *encoder\'s* loaded '
        + 'graph (the drawn route\'s tiles, which may differ from the decoder\'s tile set if no round-trip '
        + 'verify has run yet). Same shape as the decode-side get_segment.',
      parameters: {
        type: 'object',
        properties: { segment_id: { type: 'integer', description: 'Internal graph segment ID.' } },
        required: ['segment_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_encode_segments_near',
      description:
        'Find loaded road segments within radius_m of a coordinate, from the encoder\'s graph. Useful for '
        + 'inspecting the area around a waypoint that failed to snap, or around a failing leg\'s endpoints.',
      parameters: {
        type: 'object',
        properties: {
          lat:      { type: 'number', description: 'Latitude in decimal degrees.' },
          lon:      { type: 'number', description: 'Longitude in decimal degrees.' },
          radius_m: { type: 'number', description: 'Search radius in metres (max 500). Default 100.' },
        },
        required: ['lat', 'lon'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_encode_segment_neighbors',
      description:
        'All segments connected at each endpoint of a segment in the encoder\'s graph, with can_arrive/'
        + 'can_depart and turn-restriction flags. Same shape as the decode-side get_segment_neighbors. Use to '
        + 'understand junction topology around a failing connection.',
      parameters: {
        type: 'object',
        properties: { segment_id: { type: 'integer', description: 'Internal graph segment ID.' } },
        required: ['segment_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'diagnose_waypoint_connection',
      description:
        'THE primary tool for "why can\'t the encoder connect these waypoints/LRPs". Re-runs the search '
        + 'between from_node and to_node twice — once under the real turn-angle cap, once fully unrestricted — '
        + 'to tell apart genuine disconnection or wrong-direction (fails both) from being blocked specifically '
        + 'by the turn-angle gate (fails capped, succeeds unrestricted). In the latter case, returns the exact '
        + 'node(s) along the unrestricted path where the turn exceeds the cap, with the deviation in degrees. '
        + 'Omit from_node/to_node/from_segment_id to use the failing leg reported by get_encode_summary.',
      parameters: {
        type: 'object',
        properties: {
          from_node:  { type: 'integer', description: 'Internal node ID to search from. Defaults to the last encode failure\'s from_node.' },
          to_node:    { type: 'integer', description: 'Internal node ID to search to. Defaults to the last encode failure\'s to_node.' },
          from_segment_id: { type: 'integer', description: 'Optional: segment the search arrives via (seeds the first turn-angle check). Defaults to the last encode failure\'s from_segment_id, if any.' },
          max_turn_deviation_deg: { type: 'number', description: 'Optional override of the turn-angle cap. Defaults to the active max_interior_turn_deviation_deg param.' },
        },
        required: [],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'check_boundary_expansion',
      description:
        'Replay Rule-4 boundary expansion from node_id outward along segment_id (the direction *away* from '
        + 'the location) — the same walk encode_line/encode_pal perform internally whenever a location\'s '
        + 'start or end node isn\'t a real junction. Reports how far it travelled and exactly why it stopped: '
        + 'already valid, reached a valid node, ran off the loaded graph (dead end), hit max_leg_m, or was '
        + 'blocked by a turn sharper than max_turn_deviation_deg. Use when a leg fails Rule-1 (too long) or '
        + 'seems to anchor at an unexpected node.',
      parameters: {
        type: 'object',
        properties: {
          node_id:    { type: 'integer', description: 'Node to expand outward from (a location boundary).' },
          segment_id: { type: 'integer', description: 'Segment leading away from the location, in the expansion direction.' },
          max_leg_m:  { type: 'number', description: 'Optional override of the Rule-1 cap. Defaults to the active max_leg_m.' },
          max_turn_deviation_deg: { type: 'number', description: 'Optional override of the turn-angle cap. Defaults to the active param.' },
        },
        required: ['node_id', 'segment_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_turn_deviation',
      description:
        'Turn-angle deviation in degrees when transitioning from segment_a to segment_b through node_id — the '
        + 'same geometric check used by waypoint-connection A*, the coverage sweep, and Rule-4 boundary '
        + 'expansion. Returns null if either segment doesn\'t actually touch node_id.',
      parameters: {
        type: 'object',
        properties: {
          segment_a: { type: 'integer', description: 'Segment being departed.' },
          node_id:   { type: 'integer', description: 'Node where the transition happens.' },
          segment_b: { type: 'integer', description: 'Segment being entered.' },
        },
        required: ['segment_a', 'node_id', 'segment_b'],
      },
    },
  },
  ...(highlightSegmentsTool ? [highlightSegmentsTool] : []),
  ...(setMapViewTool ? [setMapViewTool] : []),
];

// ── TOON (Token-Oriented Object Notation) helpers ─────────────────────────────
// Compact tabular format: field names appear once in a header; data rows contain
// only values. Vendor-neutral — any capable LLM can parse it from the header.
//
// Format:  label[N]{col1,col2,...}:\n  v1,v2,...\n  v1,v2,...

function fmtVal(v) {
  if (v === null || v === undefined) return 'null';
  if (typeof v === 'number') return Number.isFinite(v) ? String(v) : 'null';
  return String(v);
}

// Render one TOON table: "label[N]{col,...}:\n  r1c1,r1c2,...\n  ..."
function toToon(label, rows, fields) {
  if (!rows || !rows.length) return `${label}[0]{${fields.join(',')}}:`;
  const header = `${label}[${rows.length}]{${fields.join(',')}}:`;
  const dataRows = rows.map(r => '  ' + fields.map(f => fmtVal(r[f])).join(','));
  return [header, ...dataRows].join('\n');
}

// Build a complete tool response from scalar key-value pairs and TOON tables.
// scalars: plain object — rendered as "key: value" lines (nulls omitted).
// tables:  [{ label, rows, fields }] — rendered as TOON blocks separated by blank lines.
function toonResponse(scalars, tables = []) {
  const parts = [];
  for (const [k, v] of Object.entries(scalars)) {
    if (v === null || v === undefined) continue;
    parts.push(`${k}: ${typeof v === 'object' ? JSON.stringify(v) : v}`);
  }
  for (const { label, rows, fields } of tables) {
    parts.push('');
    parts.push(toToon(label, rows, fields));
  }
  return parts.join('\n');
}

// Round to N decimal places (avoids floating-point noise in output).
function r1(v) { return v != null && Number.isFinite(v) ? Math.round(v * 10)    / 10    : null; }
function r3(v) { return v != null && Number.isFinite(v) ? Math.round(v * 1000)  / 1000  : null; }

// Extract verdict key and numeric excess from a Rust externally-tagged enum value.
// e.g. "FailBearing" → { verdict: "FailBearing", excess: null }
//      { FailBearing: { excess_deg: 33.2 } } → { verdict: "FailBearing", excess: 33.2 }
function parseVerdict(raw) {
  if (!raw || raw === 'Pass') return { verdict: 'Pass', excess: null };
  if (typeof raw === 'string') return { verdict: raw, excess: null };
  const key = Object.keys(raw)[0];
  const data = raw[key];
  const excess = data?.excess_deg ?? data?.score ?? null;
  return { verdict: key, excess: excess != null ? r1(excess) : null };
}

function resolveSourceKey(decoder, segId) {
  if (!decoder) return null;
  try { return JSON.parse(decoder.get_segment(segId))?.stable_id ?? null; } catch { return null; }
}

// ── Trace event extractor ─────────────────────────────────────────────────────

function getTraceEvents(events, variant) {
  return (events ?? [])
    .filter(e => e[variant] !== undefined)
    .map(e => e[variant]);
}

// ── Encode-side tool executor ─────────────────────────────────────────────────

async function executeEncodeTool(name, args, { encoder, encodeResult, waypoints, liveRoute, params, maxEncodeLegM, zoom, storeActions }) {
  switch (name) {

    case 'highlight_segments': {
      const { segment_ids } = args;
      if (!storeActions) return JSON.stringify({ error: 'Store actions not available.' });
      storeActions.highlightSegments(segment_ids?.length ? segment_ids : null);
      return JSON.stringify({ ok: true, highlighted: segment_ids?.length ?? 0 });
    }

    case 'set_map_view': {
      const { lat, lon, zoom: z } = args;
      if (!storeActions) return JSON.stringify({ error: 'Store actions not available.' });
      storeActions.flyTo(lat, lon, z);
      return JSON.stringify({ ok: true });
    }

    case 'get_encode_summary': {
      if (!encodeResult) return JSON.stringify({ error: 'No encode attempt yet.' });
      return toonResponse({
        ok: !encodeResult.error,
        error: encodeResult.error ?? null,
        error_from_node: encodeResult.error_from_node ?? null,
        error_to_node: encodeResult.error_to_node ?? null,
        error_from_segment_id: encodeResult.error_from_segment_id ?? null,
        v3_present: !!encodeResult.v3,
        tpeg_present: !!encodeResult.tpeg,
        waypoint_count: waypoints?.length ?? 0,
        live_route_segment_count: liveRoute?.segments?.length ?? null,
        live_route_length_m: liveRoute?.length_m != null ? r1(liveRoute.length_m) : null,
        max_leg_m: maxEncodeLegM ?? null,
        max_turn_deviation_deg: params?.max_interior_turn_deviation_deg ?? null,
      });
    }

    case 'get_waypoints': {
      const rows = (waypoints ?? []).map((w, i) => ({ index: i, lon: r3(w.lon), lat: r3(w.lat) }));
      return toonResponse({ count: rows.length }, [{ label: 'waypoints', rows, fields: ['index', 'lon', 'lat'] }]);
    }

    case 'get_encode_segment': {
      if (!encoder) return JSON.stringify({ error: 'Encoder not available.' });
      const raw = encoder.get_segment(args.segment_id);
      const data = JSON.parse(raw);
      if (data.error) return raw;
      data.frc_label = FRC_LABELS[data.frc] ?? null;
      data.fow_label = FOW_LABELS_FULL[data.fow] ?? null;
      return JSON.stringify(data);
    }

    case 'get_encode_segments_near': {
      if (!encoder) return JSON.stringify({ error: 'Encoder not available.' });
      const { lat, lon, radius_m = 100 } = args;
      const raw = encoder.get_segments_near(lat, lon, radius_m);
      const data = JSON.parse(raw);
      if (!data.segments) return raw;
      const segRows = data.segments.map(s => ({
        seg_id:    s.segment_id,
        stable_id: s.stable_id ?? null,
        frc:       s.frc,
        fow:       s.fow,
        direction: s.direction,
        length_m:  r1(s.length_m),
        dist_m:    r1(s.distance_m),
      }));
      return toonResponse(
        { lat, lon, radius_m: data.query?.radius_m ?? radius_m, count: segRows.length },
        [{ label: 'segments', rows: segRows, fields: ['seg_id', 'stable_id', 'frc', 'fow', 'direction', 'length_m', 'dist_m'] }]
      );
    }

    case 'get_encode_segment_neighbors': {
      if (!encoder) return JSON.stringify({ error: 'Encoder not available.' });
      const raw = encoder.get_segment_neighbors(args.segment_id);
      const data = JSON.parse(raw);
      if (data.error) return raw;
      const neighborFields = ['seg_id', 'stable_id', 'frc', 'fow', 'direction', 'length_m', 'can_arrive', 'can_depart'];
      const mapNeighbor = s => ({
        seg_id:     s.segment_id,
        stable_id:  s.stable_id ?? null,
        frc:        s.frc,
        fow:        s.fow,
        direction:  s.direction,
        length_m:   r1(s.length_m),
        can_arrive: s.can_arrive,
        can_depart: s.can_depart,
      });
      return toonResponse(
        { segment_id: args.segment_id, direction: data.direction, start_node: data.start_node?.node_id, end_node: data.end_node?.node_id },
        [
          { label: 'at_start_node', rows: (data.start_node?.segments ?? []).map(mapNeighbor), fields: neighborFields },
          { label: 'at_end_node',   rows: (data.end_node?.segments   ?? []).map(mapNeighbor), fields: neighborFields },
        ]
      );
    }

    case 'diagnose_waypoint_connection': {
      if (!encoder) return JSON.stringify({ error: 'Encoder not available.' });
      const fromNode = args.from_node ?? encodeResult?.error_from_node;
      const toNode   = args.to_node   ?? encodeResult?.error_to_node;
      const fromSeg  = args.from_segment_id ?? encodeResult?.error_from_segment_id ?? undefined;
      if (fromNode == null || toNode == null) {
        return JSON.stringify({
          error: 'from_node/to_node not provided and no failing leg is on record. '
            + 'Call get_encode_summary first, or pass from_node/to_node explicitly.',
        });
      }
      const maxTurn = args.max_turn_deviation_deg ?? params?.max_interior_turn_deviation_deg ?? 180;
      const raw = encoder.diagnose_connection(fromNode, toNode, fromSeg, maxTurn, zoom ?? 12);
      const diag = JSON.parse(raw);
      const sharpRows = (diag.sharp_turns ?? []).map(t => ({
        node: t.node, from_seg: t.from_segment, to_seg: t.to_segment, deviation_deg: r1(t.deviation_deg),
      }));
      return toonResponse(
        {
          from_node: fromNode,
          to_node: toNode,
          max_turn_deviation_deg: maxTurn,
          connected_within_cap: diag.connected_within_cap,
          length_within_cap_m: diag.length_within_cap_m != null ? r1(diag.length_within_cap_m) : null,
          connected_unrestricted: diag.connected_unrestricted,
          length_unrestricted_m: diag.length_unrestricted_m != null ? r1(diag.length_unrestricted_m) : null,
          blocked_by_turn_angle: diag.blocked_by_turn_angle,
          needs_tile: diag.needs_tile ? `${diag.needs_tile.z}/${diag.needs_tile.x}/${diag.needs_tile.y}` : null,
        },
        sharpRows.length ? [{ label: 'sharp_turns', rows: sharpRows, fields: ['node', 'from_seg', 'to_seg', 'deviation_deg'] }] : []
      );
    }

    case 'check_boundary_expansion': {
      if (!encoder) return JSON.stringify({ error: 'Encoder not available.' });
      const maxLeg  = args.max_leg_m ?? maxEncodeLegM ?? 15000;
      const maxTurn = args.max_turn_deviation_deg ?? params?.max_interior_turn_deviation_deg ?? 180;
      const raw = encoder.check_boundary_expansion(args.node_id, args.segment_id, maxLeg, maxTurn);
      const data = JSON.parse(raw);
      const stoppedReason = typeof data.stopped === 'string' ? data.stopped : Object.keys(data.stopped ?? {})[0] ?? null;
      const stoppedDeviation = data.stopped?.SharpTurn?.deviation_deg;
      return toonResponse(
        {
          node_id: args.node_id,
          segment_id: args.segment_id,
          max_leg_m: maxLeg,
          max_turn_deviation_deg: maxTurn,
          stopped_at_node: data.node,
          distance_m: r1(data.distance_m),
          hops: data.hops,
          still_invalid: data.still_invalid,
          stopped_reason: stoppedReason,
          stopped_deviation_deg: stoppedDeviation != null ? r1(stoppedDeviation) : null,
        },
        data.segments?.length
          ? [{ label: 'segments_walked', rows: data.segments.map((s, i) => ({ step: i, seg_id: s })), fields: ['step', 'seg_id'] }]
          : []
      );
    }

    case 'get_turn_deviation': {
      if (!encoder) return JSON.stringify({ error: 'Encoder not available.' });
      const raw = encoder.get_turn_deviation(args.segment_a, args.node_id, args.segment_b);
      const data = JSON.parse(raw);
      return JSON.stringify({ deviation_deg: data.deviation_deg != null ? r1(data.deviation_deg) : null });
    }

    default:
      return JSON.stringify({ error: `Encode tool "${name}" is not yet implemented in this version.` });
  }
}

// ── Tool executor ─────────────────────────────────────────────────────────────

const ENCODE_TOOL_NAMES = new Set(ENCODE_TOOL_DEFINITIONS.map(t => t.function.name));

// storeActions: { setPinnedCandidates, runForcedDecodeAndGet, highlightSegments, flyTo }
export async function executeTool(name, args, ctx) {
  const { decodeResult, params, decoder, storeActions, forcedDecodeResult, encoder, encodeResult, waypoints, liveRoute, maxEncodeLegM, zoom } = ctx;

  // Encode-side tools (including the shared highlight_segments/set_map_view)
  // never need `decodeResult` — dispatch them before the decode-only guard
  // below, which would otherwise reject them whenever nothing has been
  // decoded/verified yet (the common case while an encode is still failing).
  if (ENCODE_TOOL_NAMES.has(name)) {
    return executeEncodeTool(name, args, { encoder, encodeResult, waypoints, liveRoute, params, maxEncodeLegM, zoom, storeActions });
  }

  if (!decodeResult) return JSON.stringify({ error: 'No decode result available.' });

  // When a forced decode is active, routing tools (leg summary, route segments, skipped edges)
  // should reflect the forced route, not the original multi-attempt trace.
  const isForced = !!forcedDecodeResult?.ok;
  const activeResult = isForced ? forcedDecodeResult : decodeResult;
  const routingEvents = activeResult.trace?.events ?? [];
  // Original trace is still needed for candidate tools and attempted-combinations.
  const events = decodeResult.trace?.events ?? [];

  switch (name) {

    case 'get_decode_summary': {
      const segs = activeResult.segments ?? [];
      const totalLengthM = segs.reduce((sum, s) => sum + (s.length_m ?? 0), 0);

      const scalars = {
        ok:                    activeResult.ok,
        forced_decode_active:  isForced,
        format:                decodeResult.format ?? null,
        error:                 activeResult.error  ?? null,
        segment_count:         segs.length,
        lrp_count:             decodeResult.lrps?.length ?? 0,
        path_total_length_m:   r1(totalLengthM),
        pos_offset_m:          decodeResult.pos_offset_lb != null
          ? `[${decodeResult.pos_offset_lb},${decodeResult.pos_offset_ub}]` : null,
        neg_offset_m:          decodeResult.neg_offset_lb != null
          ? `[${decodeResult.neg_offset_lb},${decodeResult.neg_offset_ub}]` : null,
        search_radius_m:       params?.candidate_search_radius_m,
        bearing_tolerance_deg: params?.max_bearing_deviation_deg,
        dnp_tolerance_pct:     params?.dnp_tolerance_pct,
        lfrcnp_tolerance:      params?.lfrcnp_tolerance,
        max_candidate_score:   params?.max_candidate_score,
        max_candidates_per_lrp: params?.max_candidates_per_lrp,
      };

      const pathRows = segs.map(s => ({
        seg_id:     s.segment_id,
        stable_id: s.stable_id ?? s.stable_id ?? null,
        frc:        s.frc,
        fow:        s.fow,
        direction:  s.direction,
        length_m:   r1(s.length_m),
      }));

      return toonResponse(scalars,
        pathRows.length
          ? [{ label: 'path', rows: pathRows, fields: ['seg_id','stable_id','frc','fow','direction','length_m'] }]
          : []
      );
    }

    case 'get_parsed_reference': {
      const lrps = (decodeResult.lrps ?? []).map((l, i) => {
        const isLast = i === decodeResult.lrps.length - 1;
        return {
          index: i,
          lat: l.lat,
          lon: l.lon,
          bearing: { lb: l.bearing_lb, ub: l.bearing_ub },
          frc: l.frc,
          frc_label: FRC_LABELS[l.frc] ?? null,
          fow: l.fow,
          fow_label: FOW_LABELS[l.fow] ?? null,
          lfrcnp: isLast ? null : l.lfrcnp,
          dnp_m: isLast ? null
            : l.dnp_lb != null ? { lb: l.dnp_lb, ub: l.dnp_ub ?? l.dnp_lb }
            : null,
        };
      });
      return JSON.stringify({ lrps });
    }

    case 'get_lrp_candidates': {
      const { lrp_index, include_rejected = false } = args;
      const idx0 = lrp_index - 1;
      const ranked = getTraceEvents(events, 'CandidatesRanked');
      const data = ranked.find(e => e.lrp_idx === idx0);
      if (!data) return JSON.stringify({ error: `No candidate trace data for LRP ${lrp_index}.` });

      const acceptedRows = (data.accepted ?? []).map(c => ({
        seg_id:      c.segment_id,
        stable_id:  resolveSourceKey(decoder, c.segment_id),
        traversal:   c.traversal,
        dist_m:      r1(c.projection?.distance_m),
        bearing_deg: r1(c.projection?.bearing_deg),
        dist_sc:     r3(c.score?.distance_score),
        bear_sc:     r3(c.score?.bearing_score),
        frc_sc:      r3(c.score?.frc_score),
        fow_sc:      r3(c.score?.fow_score),
        total:       r3(c.score?.total),
      }));

      const scalars = {
        lrp_index,
        accepted_count: acceptedRows.length,
        rejected_count: data.rejected_count ?? data.rejected?.length ?? 0,
      };

      const tables = [{
        label:  'accepted',
        rows:   acceptedRows,
        fields: ['seg_id','stable_id','traversal','dist_m','bearing_deg','dist_sc','bear_sc','frc_sc','fow_sc','total'],
      }];

      if (include_rejected) {
        const rejectedRows = (data.rejected ?? []).map(r => {
          const { verdict, excess } = parseVerdict(r.verdict);
          return {
            seg_id:      r.segment_id,
            stable_id:  resolveSourceKey(decoder, r.segment_id),
            dist_m:      r.projection?.distance_m != null ? r1(r.projection.distance_m) : null,
            bearing_deg: r.projection?.bearing_deg != null ? r1(r.projection.bearing_deg) : null,
            verdict,
            excess,
          };
        });
        tables.push({
          label:  'rejected',
          rows:   rejectedRows,
          fields: ['seg_id','stable_id','dist_m','bearing_deg','verdict','excess'],
        });
      }

      return toonResponse(scalars, tables);
    }

    case 'get_leg_summary': {
      const { leg_index } = args;
      const idx0 = leg_index - 1;
      const routing = {};
      for (const ev of routingEvents) {
        const [type, data] = Object.entries(ev)[0];
        if (data.leg !== idx0) continue;
        switch (type) {
          case 'RouteFound':      routing.result = { found: true,  ...data }; break;
          case 'RouteFailed':     routing.result = { found: false, ...data }; break;
          case 'DnpChecked':      routing.dnp    = data;                      break;
          case 'AStarTerminated': routing.astar  = data;                      break;
          default: break;
        }
      }
      if (!Object.keys(routing).length) {
        return JSON.stringify({ error: `No routing trace data for leg ${leg_index}.` });
      }
      const r = routing.result;
      const d = routing.dnp;
      const a = routing.astar;
      return JSON.stringify({
        leg_index,
        route_found:       r?.found ?? null,
        route_length_m:    r?.found ? r.length_m : null,
        route_fail_reason: r?.found === false ? r.reason : null,
        dnp: d ? {
          actual_m:  d.actual_m,
          window_lb: d.interval?.lb,
          window_ub: d.interval?.ub,
          passed:    d.passed,
        } : null,
        astar: a ? {
          nodes_expanded:          a.nodes_expanded,
          edges_skipped_frc:       a.edges_skipped_frc,
          edges_skipped_direction: a.edges_skipped_direction,
          edges_skipped_turn:      a.edges_skipped_turn,
          edges_skipped_distance:  a.edges_skipped_distance,
          edges_skipped_sharp_turn: a.edges_skipped_sharp_turn,
          reason: a.reason,
        } : null,
      });
    }

    case 'get_route_segments': {
      const { leg_index } = args;
      const idx0 = leg_index - 1;
      const found = getTraceEvents(routingEvents, 'RouteFound');
      const data = found.find(e => e.leg === idx0);
      if (!data) return JSON.stringify({ error: `No successful route found for leg ${leg_index}.` });

      const segById = new Map((activeResult.segments ?? []).map(s => [s.segment_id, s]));
      let cumul = 0;
      const segRows = (data.path ?? []).map(id => {
        const info = segById.get(id);
        const len = info?.length_m ?? null;
        if (len != null) cumul += len;
        return {
          seg_id:    id,
          stable_id: info?.stable_id ?? info?.stable_id ?? resolveSourceKey(decoder, id),
          frc:       info?.frc       ?? null,
          fow:       info?.fow       ?? null,
          direction: info?.direction ?? null,
          length_m:  r1(len),
          cumul_m:   r1(cumul),
        };
      });
      const sumLengthM = segRows.reduce((s, seg) => s + (seg.length_m ?? 0), 0);

      const fromSnap = data.from_snap;
      const toSnap   = data.to_snap;

      return toonResponse(
        {
          leg_index,
          segment_count: segRows.length,
          length_m:      r1(data.length_m),
          segment_sum_m: r1(sumLengthM),
          from_snap: fromSnap ? `${fromSnap[0]},${fromSnap[1]}` : null,
          to_snap:   toSnap   ? `${toSnap[0]},${toSnap[1]}`     : null,
        },
        [{ label: 'segments', rows: segRows, fields: ['seg_id','stable_id','frc','fow','direction','length_m','cumul_m'] }]
      );
    }

    case 'get_segment_neighbors': {
      const { segment_id } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const raw = decoder.get_segment_neighbors(segment_id);
      const data = JSON.parse(raw);
      if (data.error) return raw;

      const neighborFields = ['seg_id','stable_id','frc','fow','direction','length_m','can_arrive','can_depart'];
      const mapNeighbor = s => ({
        seg_id:     s.segment_id,
        stable_id: s.stable_id ?? null,
        frc:        s.frc,
        fow:        s.fow,
        direction:  s.direction,
        length_m:   r1(s.length_m),
        can_arrive: s.can_arrive,
        can_depart: s.can_depart,
      });

      return toonResponse(
        {
          segment_id,
          direction:  data.direction,
          start_node: data.start_node?.node_id,
          end_node:   data.end_node?.node_id,
        },
        [
          { label: 'at_start_node', rows: (data.start_node?.segments ?? []).map(mapNeighbor), fields: neighborFields },
          { label: 'at_end_node',   rows: (data.end_node?.segments   ?? []).map(mapNeighbor), fields: neighborFields },
        ]
      );
    }

    case 'get_segment': {
      const { segment_id } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const raw = decoder.get_segment(segment_id);
      const data = JSON.parse(raw);
      if (data.error) return raw;
      data.frc_label = FRC_LABELS[data.frc] ?? null;
      data.fow_label = FOW_LABELS_FULL[data.fow] ?? null;
      return JSON.stringify(data);
    }

    case 'get_segments_near': {
      const { lat, lon, radius_m = 100 } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const raw = decoder.get_segments_near(lat, lon, radius_m);
      const data = JSON.parse(raw);
      if (!data.segments) return raw;

      const segRows = data.segments.map(s => ({
        seg_id:     s.segment_id,
        stable_id: s.stable_id ?? null,
        frc:        s.frc,
        fow:        s.fow,
        direction:  s.direction,
        length_m:   r1(s.length_m),
        dist_m:     r1(s.distance_m),
      }));

      return toonResponse(
        { lat, lon, radius_m: data.query?.radius_m ?? radius_m, count: segRows.length },
        [{ label: 'segments', rows: segRows, fields: ['seg_id','stable_id','frc','fow','direction','length_m','dist_m'] }]
      );
    }

    case 'highlight_segments': {
      const { segment_ids } = args;
      if (!storeActions) return JSON.stringify({ error: 'Store actions not available.' });
      storeActions.highlightSegments(segment_ids?.length ? segment_ids : null);
      return JSON.stringify({ ok: true, highlighted: segment_ids?.length ?? 0 });
    }

    case 'set_map_view': {
      const { lat, lon, zoom } = args;
      if (!storeActions) return JSON.stringify({ error: 'Store actions not available.' });
      storeActions.flyTo(lat, lon, zoom);
      return JSON.stringify({ ok: true });
    }

    case 'focus_lrp': {
      const { lrp_index } = args;
      if (!storeActions) return JSON.stringify({ error: 'Store actions not available.' });
      const lrp = decodeResult.lrps?.[lrp_index - 1];
      if (!lrp) return JSON.stringify({ error: `LRP ${lrp_index} not found.` });
      storeActions.flyTo(lrp.lat, lrp.lon, 16);
      return JSON.stringify({ ok: true, lat: lrp.lat, lon: lrp.lon });
    }

    case 'get_astar_skipped_edges': {
      const { leg_index, segment_id } = args;
      const idx0 = leg_index - 1;
      const skipped = getTraceEvents(routingEvents, 'AStarEdgeSkipped').filter(e => e.leg === idx0);
      if (!skipped.length) {
        const hasFullTrace = routingEvents.some(e => e.AStarEdgeSkipped !== undefined || e.AStarNodeExpanded !== undefined);
        if (!hasFullTrace) return JSON.stringify({ error: 'Full trace required. Set Trace Level → Full and decode again.' });
        return JSON.stringify({ leg_index, count: 0, note: 'No edges skipped on this leg.' });
      }
      const filtered = segment_id != null ? skipped.filter(e => e.segment_id === segment_id) : skipped;
      const rows = filtered.map(e => {
        const r = e.reason ?? {};
        const rKey = typeof r === 'string' ? r : Object.keys(r)[0] ?? 'Unknown';
        const rData = typeof r === 'object' ? r[rKey] : null;
        return {
          seg_id:  e.segment_id,
          reason:  rKey,
          seg_frc: rData?.seg_frc ?? null,
          lfrcnp:  rData?.lfrcnp  ?? null,
          dist_m:  rData?.distance_m != null ? r1(rData.distance_m) : null,
          max_m:   rData?.max_m    != null ? r1(rData.max_m)     : null,
        };
      });
      return toonResponse(
        { leg_index, total_skipped: skipped.length, shown: rows.length },
        [{ label: 'skipped', rows, fields: ['seg_id','reason','seg_frc','lfrcnp','dist_m','max_m'] }]
      );
    }

    case 'get_forced_leg_summary': {
      const { leg_index } = args;
      const idx0 = leg_index - 1;
      if (!forcedDecodeResult) return JSON.stringify({ error: 'No forced decode result. Call run_forced_decode first.' });
      const fEvents = forcedDecodeResult.trace?.events ?? [];
      const routing = {};
      for (const ev of fEvents) {
        const [type, data] = Object.entries(ev)[0];
        if (data.leg !== idx0) continue;
        switch (type) {
          case 'RouteFound':      routing.result = { found: true,  ...data }; break;
          case 'RouteFailed':     routing.result = { found: false, ...data }; break;
          case 'DnpChecked':      routing.dnp    = data;                      break;
          case 'AStarTerminated': routing.astar  = data;                      break;
          default: break;
        }
      }
      if (!Object.keys(routing).length) return JSON.stringify({ error: `No routing trace for forced leg ${leg_index}.` });
      const r = routing.result, d = routing.dnp, a = routing.astar;
      return JSON.stringify({
        leg_index,
        route_found:       r?.found ?? null,
        route_length_m:    r?.found ? r.length_m : null,
        route_fail_reason: r?.found === false ? r.reason : null,
        dnp: d ? { actual_m: d.actual_m, window_lb: d.interval?.lb, window_ub: d.interval?.ub, passed: d.passed } : null,
        astar: a ? {
          nodes_expanded:          a.nodes_expanded,
          edges_skipped_frc:       a.edges_skipped_frc,
          edges_skipped_direction: a.edges_skipped_direction,
          edges_skipped_turn:      a.edges_skipped_turn,
          edges_skipped_distance:  a.edges_skipped_distance,
          edges_skipped_sharp_turn: a.edges_skipped_sharp_turn,
          reason: a.reason,
        } : null,
      });
    }

    case 'get_attempted_combinations': {
      const started = getTraceEvents(events, 'RouteSearchStarted');
      const exhausted = getTraceEvents(events, 'RouteAttemptsExhausted')[0] ?? null;
      // Group by combination: a new combination starts each time leg resets to 0 (or first event)
      const combos = [];
      let current = null;
      for (const s of started) {
        if (s.leg === 0 || current === null) { current = { legs: [] }; combos.push(current); }
        // Find outcome event for this specific leg search (next RouteFound/RouteFailed for this leg after this start)
        current.legs.push({ leg: s.leg + 1, from_seg: s.from.segment_id, from_trav: s.from.traversal, to_seg: s.to.segment_id, to_trav: s.to.traversal });
      }
      // Match outcomes: RouteFound/RouteFailed events per leg
      const foundEvts  = getTraceEvents(events, 'RouteFound');
      const failedEvts = getTraceEvents(events, 'RouteFailed');
      const rows = started.map((s, idx) => {
        // Find matching outcome by leg — simplified: same leg order in events
        const rf = foundEvts.find((e, i) => e.leg === s.leg && i === started.slice(0, idx + 1).filter(x => x.leg === s.leg).length - 1);
        const fail = failedEvts.find((e, i) => e.leg === s.leg && i === started.slice(0, idx + 1).filter(x => x.leg === s.leg).length - 1);
        const outcome = rf ? `found(${r1(rf.length_m)}m)` : fail ? `failed` : 'incomplete';
        return { leg: s.leg + 1, from_seg: s.from.segment_id, from_trav: s.from.traversal, to_seg: s.to.segment_id, to_trav: s.to.traversal, outcome };
      });
      return toonResponse(
        { total_attempts: started.length, cap_hit: exhausted != null, cap_limit: exhausted?.limit ?? null },
        [{ label: 'attempts', rows, fields: ['leg','from_seg','from_trav','to_seg','to_trav','outcome'] }]
      );
    }

    case 'retry_decode': {
      const { params_override } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const overrideStr = typeof params_override === 'string'
        ? params_override
        : JSON.stringify(params_override);
      return decoder.retry_decode(overrideStr);
    }

    case 'set_pinned_candidates': {
      const { snaps } = args;
      if (!storeActions) return JSON.stringify({ error: 'Store actions not available.' });
      const ranked = getTraceEvents(events, 'CandidatesRanked');
      const resolved = [];
      const errs = [];
      for (const { lrp_index, segment_id, traversal } of snaps) {
        const idx0 = lrp_index - 1;
        const lrpData = ranked.find(e => e.lrp_idx === idx0);
        if (!lrpData) { errs.push(`LRP ${lrp_index}: no candidate trace data`); continue; }
        const c = (lrpData.accepted ?? []).find(
          a => a.segment_id === segment_id && a.traversal === traversal
        );
        if (!c) { errs.push(`LRP ${lrp_index}: segment ${segment_id} (${traversal}) not in accepted list`); continue; }
        resolved.push({
          lrp_index: idx0,
          segment_id:   c.segment_id,
          traversal:    c.traversal,
          arc_offset_m: c.projection.arc_offset_m,
          snap_lon:     c.projection.point[0],
          snap_lat:     c.projection.point[1],
        });
      }
      if (errs.length) return JSON.stringify({ error: errs.join('; ') });
      await storeActions.setPinnedCandidates(resolved);
      return JSON.stringify({ ok: true, pinned_count: resolved.length });
    }

    case 'run_forced_decode': {
      if (!storeActions) return JSON.stringify({ error: 'Store actions not available.' });
      const result = await storeActions.runForcedDecodeAndGet();
      if (!result) return JSON.stringify({ error: 'Not all LRPs pinned. Call set_pinned_candidates first.' });
      if (!result.ok) return JSON.stringify({ ok: false, error: result.error ?? 'Forced decode failed.' });
      const segs = result.segments ?? [];
      const totalLengthM = segs.reduce((s, seg) => s + (seg.length_m ?? 0), 0);
      const forcedEvents = result.trace?.events ?? [];
      const legResults = getTraceEvents(forcedEvents, 'DnpChecked').map(d => ({
        leg:        d.leg + 1,
        actual_m:   r1(d.actual_m),
        window_lb:  r1(d.interval?.lb),
        window_ub:  r1(d.interval?.ub),
        dnp_passed: d.passed,
      }));
      return toonResponse(
        { ok: true, segment_count: segs.length, path_total_length_m: r1(totalLengthM) },
        legResults.length
          ? [{ label: 'legs', rows: legResults, fields: ['leg','actual_m','window_lb','window_ub','dnp_passed'] }]
          : []
      );
    }

    case 'get_route_geometry': {
      const segs = activeResult.segments ?? [];
      if (!segs.length) return JSON.stringify({ error: 'No path segments — decode first.' });

      const allPts = segs.flatMap(s => s.geometry ?? []);
      if (!allPts.length) return JSON.stringify({ error: 'No geometry in path segments.' });

      let minLon = Infinity, maxLon = -Infinity, minLat = Infinity, maxLat = -Infinity;
      for (const [lon, lat] of allPts) {
        if (lon < minLon) minLon = lon;  if (lon > maxLon) maxLon = lon;
        if (lat < minLat) minLat = lat;  if (lat > maxLat) maxLat = lat;
      }

      const W = 540, H = 290, PAD = 28;
      const lonRange = maxLon - minLon || 0.001;
      const latRange = maxLat - minLat || 0.001;
      const midLat   = (minLat + maxLat) / 2;
      const scale    = Math.min((W - 2 * PAD) / lonRange, (H - 2 * PAD) / latRange);
      const offX     = PAD + ((W - 2 * PAD) - lonRange * scale) / 2;
      const offY     = PAD + ((H - 2 * PAD) - latRange * scale) / 2;
      const project  = (lon, lat) => [
        Math.round(offX + (lon - minLon) * scale),
        Math.round(offY + (maxLat - lat) * scale),
      ];

      const totalPts = allPts.length;
      const step = totalPts > 400 ? Math.ceil(totalPts / 400) : 1;

      let pathD = '';
      let gIdx = 0;
      for (const seg of segs) {
        const geom = seg.geometry ?? [];
        for (let i = 0; i < geom.length; i++, gIdx++) {
          if (i > 0 && gIdx % step !== 0 && i !== geom.length - 1) continue;
          const [x, y] = project(geom[i][0], geom[i][1]);
          pathD += (pathD === '' ? `M${x},${y}` : `L${x},${y}`);
        }
      }

      const lrpMarkers = (decodeResult.lrps ?? []).map((l, i) => {
        if (l.snap_lon == null) return '';
        const [x, y] = project(l.snap_lon, l.snap_lat);
        const isLast = i === decodeResult.lrps.length - 1;
        const fill = i === 0 ? '#4caf50' : isLast ? '#f44336' : '#ff9800';
        return `<circle cx="${x}" cy="${y}" r="5" fill="${fill}" stroke="#fff" stroke-width="1.5"/>`
          + `<text x="${x}" y="${y - 8}" text-anchor="middle" fill="${fill}" font-size="9" font-family="sans-serif">L${i + 1}</text>`;
      }).join('');

      const metersPerLonDeg = 111320 * Math.cos(midLat * Math.PI / 180);
      const pixPerMeter     = scale / metersPerLonDeg;
      const rawBarM         = 80 / pixPerMeter;
      const exp             = Math.floor(Math.log10(rawBarM));
      const mantissa        = rawBarM / Math.pow(10, exp);
      const niceMantissa    = mantissa < 1.5 ? 1 : mantissa < 3.5 ? 2 : 5;
      const barM            = niceMantissa * Math.pow(10, exp);
      const barPx           = Math.round(barM * pixPerMeter);
      const bx = W - PAD, by = H - 12;
      const barLabel = barM >= 1000 ? `${barM / 1000} km` : `${barM} m`;
      const scaleBar = `<line x1="${bx - barPx}" y1="${by}" x2="${bx}" y2="${by}" stroke="#777" stroke-width="1.5"/>`
        + `<line x1="${bx - barPx}" y1="${by - 3}" x2="${bx - barPx}" y2="${by + 3}" stroke="#777" stroke-width="1.5"/>`
        + `<line x1="${bx}" y1="${by - 3}" x2="${bx}" y2="${by + 3}" stroke="#777" stroke-width="1.5"/>`
        + `<text x="${bx - barPx / 2}" y="${by - 5}" text-anchor="middle" fill="#777" font-size="9" font-family="sans-serif">${barLabel}</text>`;

      return JSON.stringify({
        width: W, height: H,
        route_path:  `<path d="${pathD}" stroke="#4aaa88" stroke-width="2" fill="none"/>`,
        lrp_markers: lrpMarkers,
        scale_bar:   scaleBar,
        note:        `Wrap in: <svg width="${W}" height="${H}" xmlns="http://www.w3.org/2000/svg"><rect width="${W}" height="${H}" fill="#111"/>…route_path…lrp_markers…scale_bar…</svg>`,
      });
    }

    case 'check_path_feasibility': {
      const { leg_index, segment_ids } = args;
      const idx0 = leg_index - 1;
      if (!Array.isArray(segment_ids) || segment_ids.length === 0)
        return JSON.stringify({ error: 'segment_ids must be a non-empty array.' });
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });

      const lrp = decodeResult.lrps?.[idx0];
      if (!lrp) return JSON.stringify({ error: `No LRP at index ${leg_index}.` });

      const lfrcnp = (lrp.lfrcnp ?? 7) + (params?.lfrcnp_tolerance ?? 0);
      const segData = segment_ids.map(id => JSON.parse(decoder.get_segment(id)));

      const rows = [];
      let feasible = true;

      // Check first segment's LFRCNP
      const first = segData[0];
      if (first?.error) {
        rows.push({ step: 0, from_seg: 'entry', to_seg: segment_ids[0], node: null, status: 'error', reason: 'SegmentNotLoaded' });
        feasible = false;
      } else if (first?.frc > lfrcnp) {
        rows.push({ step: 0, from_seg: 'entry', to_seg: segment_ids[0], node: null, status: 'blocked', reason: `FrcBelowLfrcnp:FRC${first.frc}>LFRCNP${lfrcnp}` });
        feasible = false;
      }

      for (let i = 0; i < segment_ids.length - 1; i++) {
        const a = segData[i], b = segData[i + 1];
        const aId = segment_ids[i], bId = segment_ids[i + 1];

        if (a?.error || b?.error) {
          rows.push({ step: i + 1, from_seg: aId, to_seg: bId, node: null, status: 'error', reason: 'SegmentNotLoaded' });
          feasible = false;
          continue;
        }

        if (b.frc > lfrcnp) {
          rows.push({ step: i + 1, from_seg: aId, to_seg: bId, node: null, status: 'blocked', reason: `FrcBelowLfrcnp:FRC${b.frc}>LFRCNP${lfrcnp}` });
          feasible = false;
          continue;
        }

        // Find shared node
        let sharedNode = null, aExitEnd = null;
        if      (a.end_node   === b.start_node) { sharedNode = a.end_node;   aExitEnd = 'end'; }
        else if (a.end_node   === b.end_node)   { sharedNode = a.end_node;   aExitEnd = 'end'; }
        else if (a.start_node === b.start_node) { sharedNode = a.start_node; aExitEnd = 'start'; }
        else if (a.start_node === b.end_node)   { sharedNode = a.start_node; aExitEnd = 'start'; }

        if (!sharedNode) {
          rows.push({ step: i + 1, from_seg: aId, to_seg: bId, node: null, status: 'blocked', reason: 'NotConnected' });
          feasible = false;
          continue;
        }

        const aCanExit  = a.direction === 'Both'
          || (a.direction === 'Forward'  && aExitEnd === 'end')
          || (a.direction === 'Backward' && aExitEnd === 'start');

        if (!aCanExit) {
          rows.push({ step: i + 1, from_seg: aId, to_seg: bId, node: sharedNode, status: 'blocked', reason: `WrongDirection:${aId}(${a.direction})cannotExit` });
          feasible = false;
          continue;
        }

        const bEnterEnd = sharedNode === b.start_node ? 'start' : 'end';
        const bCanEnter = b.direction === 'Both'
          || (b.direction === 'Forward'  && bEnterEnd === 'start')
          || (b.direction === 'Backward' && bEnterEnd === 'end');

        if (!bCanEnter) {
          rows.push({ step: i + 1, from_seg: aId, to_seg: bId, node: sharedNode, status: 'blocked', reason: `WrongDirection:${bId}(${b.direction})cannotEnter` });
          feasible = false;
          continue;
        }

        const nbr       = JSON.parse(decoder.get_segment_neighbors(aId));
        const nodeGroup = aExitEnd === 'start' ? nbr.start_node : nbr.end_node;
        const nbrEntry  = (nodeGroup?.segments ?? []).find(s => s.segment_id === bId);
        if (nbrEntry?.restricted_from_self) {
          rows.push({ step: i + 1, from_seg: aId, to_seg: bId, node: sharedNode, status: 'blocked', reason: 'TurnRestriction' });
          feasible = false;
          continue;
        }

        rows.push({ step: i + 1, from_seg: aId, to_seg: bId, node: sharedNode, status: 'ok', reason: null });
      }

      const fb = rows.find(r => r.status === 'blocked');
      return toonResponse(
        { leg_index, lfrcnp_effective: lfrcnp, segment_count: segment_ids.length, feasible, first_blockage: fb ? `${fb.reason} at step ${fb.step}` : null },
        rows.length ? [{ label: 'steps', rows, fields: ['step','from_seg','to_seg','node','status','reason'] }] : []
      );
    }

    case 'score_path': {
      const { leg_index, segment_ids } = args;
      const idx0 = leg_index - 1;
      if (!Array.isArray(segment_ids) || segment_ids.length === 0)
        return JSON.stringify({ error: 'segment_ids must be a non-empty array.' });
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });

      const lrp = decodeResult.lrps?.[idx0];
      if (!lrp) return JSON.stringify({ error: `No LRP at index ${leg_index}.` });

      const routeFoundEvt = getTraceEvents(routingEvents, 'RouteFound').find(e => e.leg === idx0);
      const actualLengthM = routeFoundEvt?.length_m ?? null;

      const dnpEvt  = getTraceEvents(routingEvents, 'DnpChecked').find(e => e.leg === idx0);
      const windowLb = dnpEvt?.interval?.lb ?? null;
      const windowUb = dnpEvt?.interval?.ub ?? null;

      let totalLength = 0;
      const segRows = [];
      for (const seg_id of segment_ids) {
        const s = JSON.parse(decoder.get_segment(seg_id));
        if (s.error) { segRows.push({ seg_id, stable_id: null, frc: null, length_m: null }); continue; }
        const len = s.length_m ?? 0;
        totalLength += len;
        segRows.push({ seg_id, stable_id: s.stable_id ?? null, frc: s.frc, length_m: r1(len) });
      }

      const dnpPasses = windowLb != null ? totalLength >= windowLb && totalLength <= windowUb : null;

      return toonResponse(
        {
          leg_index,
          proposed_length_m:  r1(totalLength),
          actual_length_m:    r1(actualLengthM),
          delta_m:            actualLengthM != null ? r1(totalLength - actualLengthM) : null,
          dnp_window:         windowLb != null ? `[${r1(windowLb)},${r1(windowUb)}]` : null,
          dnp_raw_encoded:    lrp.dnp_lb != null ? `[${r1(lrp.dnp_lb)},${r1(lrp.dnp_ub ?? lrp.dnp_lb)}]` : null,
          dnp_passes:         dnpPasses,
        },
        [{ label: 'segments', rows: segRows, fields: ['seg_id','stable_id','frc','length_m'] }]
      );
    }

    case 'get_junction_topology': {
      const { node_id, hint_segment_id } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });

      // Resolve a segment that touches node_id
      let hintId = hint_segment_id ?? null;
      if (!hintId) {
        // Scan the decoded route path for a touching segment (cap at 60 to limit WASM calls)
        const pathSegs  = getTraceEvents(routingEvents, 'RouteFound').flatMap(e => e.path ?? []);
        const knownSegs = (activeResult.segments ?? []).map(s => s.segment_id).filter(Boolean);
        const candidates = [...new Set([...pathSegs, ...knownSegs])].slice(0, 60);
        for (const sid of candidates) {
          const s = JSON.parse(decoder.get_segment(sid));
          if (s.start_node === node_id || s.end_node === node_id) { hintId = sid; break; }
        }
      }

      if (!hintId) {
        return JSON.stringify({
          error: `No loaded segment found touching node ${node_id}. Pass hint_segment_id (any segment ID known to touch this node) to bypass the scan.`,
        });
      }

      const hintSeg = JSON.parse(decoder.get_segment(hintId));
      if (hintSeg.error) return JSON.stringify({ error: hintSeg.error });
      if (hintSeg.start_node !== node_id && hintSeg.end_node !== node_id)
        return JSON.stringify({ error: `hint_segment_id ${hintId} does not touch node ${node_id}.` });

      const isAtStart = hintSeg.start_node === node_id;
      const nbr       = JSON.parse(decoder.get_segment_neighbors(hintId));
      if (nbr.error) return JSON.stringify({ error: nbr.error });

      const nodeGroup = isAtStart ? nbr.start_node : nbr.end_node;
      const geom      = hintSeg.geometry ?? [];
      const nodeCoord = isAtStart ? geom[0] : geom[geom.length - 1];

      const fields = ['seg_id','stable_id','frc','fow','direction','length_m','can_arrive','can_depart','restricted_from_self','restricted_into_self'];
      const rows = (nodeGroup?.segments ?? []).map(s => ({
        seg_id:               s.segment_id,
        stable_id:           s.stable_id ?? null,
        frc:                  s.frc,
        fow:                  s.fow,
        direction:            s.direction,
        length_m:             r1(s.length_m),
        can_arrive:           s.can_arrive,
        can_depart:           s.can_depart,
        restricted_from_self: s.restricted_from_self ?? false,
        restricted_into_self: s.restricted_into_self ?? false,
      }));

      return toonResponse(
        { node_id, node_lon: nodeCoord ? r3(nodeCoord[0]) : null, node_lat: nodeCoord ? r3(nodeCoord[1]) : null, segment_count: rows.length },
        rows.length ? [{ label: 'segments', rows, fields }] : []
      );
    }

    case 'get_bearing_geometry': {
      const { lrp_index, segment_id } = args;
      const idx0 = lrp_index - 1;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });

      const lrp = decodeResult.lrps?.[idx0];
      if (!lrp) return JSON.stringify({ error: `No LRP at index ${lrp_index}.` });

      const rankedEvent = getTraceEvents(events, 'CandidatesRanked').find(e => e.lrp_idx === idx0);
      if (!rankedEvent) return JSON.stringify({ error: `No candidate trace data for LRP ${lrp_index}. Ensure trace level is Summary or Full.` });

      const accepted   = rankedEvent.accepted ?? [];
      const rejected   = rankedEvent.rejected ?? [];
      const candidate  = accepted.find(c => c.segment_id === segment_id)
        ?? rejected.find(c => c.segment_id === segment_id);
      if (!candidate) return JSON.stringify({ error: `Segment ${segment_id} not found as a candidate for LRP ${lrp_index}.` });

      const isAccepted  = accepted.some(c => c.segment_id === segment_id);
      const proj        = candidate.projection ?? {};
      const arcOffsetM  = proj.arc_offset_m ?? 0;
      const snapPt      = proj.point ?? [null, null];
      const bearingDeg  = proj.bearing_deg ?? null;

      const segRaw = decoder.get_segment(segment_id);
      const segData = JSON.parse(segRaw);
      if (segData.error) return JSON.stringify({ error: segData.error });

      // Trim geometry to ±60 m around the snap point
      const geom = segData.geometry ?? [];
      const windowGeom = [];
      let cumDist = 0;
      for (let i = 0; i < geom.length; i++) {
        if (i > 0) {
          const lat = (geom[i][1] + geom[i - 1][1]) / 2;
          const dx  = (geom[i][0] - geom[i - 1][0]) * 111320 * Math.cos(lat * Math.PI / 180);
          const dy  = (geom[i][1] - geom[i - 1][1]) * 111320;
          cumDist  += Math.sqrt(dx * dx + dy * dy);
        }
        if (Math.abs(cumDist - arcOffsetM) <= 60) windowGeom.push([r3(geom[i][0]), r3(geom[i][1])]);
      }
      if (!windowGeom.length) geom.slice(0, 5).forEach(p => windowGeom.push([r3(p[0]), r3(p[1])]));

      const bearingLb = lrp.bearing_lb ?? 0;
      const bearingUb = lrp.bearing_ub ?? 360;
      const bearingTol = params?.max_bearing_deviation_deg ?? 0;
      // Normalise to [0,360)
      const norm = v => ((v % 360) + 360) % 360;
      const effectiveLb = norm(bearingLb - bearingTol);
      const effectiveUb = norm(bearingUb + bearingTol);

      const { verdict, excess } = parseVerdict(candidate.verdict);

      return JSON.stringify({
        lrp_index,
        segment_id,
        stable_id:           segData.stable_id ?? null,
        accepted:             isAccepted,
        verdict,
        excess_deg:           excess,
        bearing_deg:          r1(bearingDeg),
        encoded_lb:           r1(bearingLb),
        encoded_ub:           r1(bearingUb),
        tolerance_deg:        bearingTol,
        effective_lb:         r1(effectiveLb),
        effective_ub:         r1(effectiveUb),
        snap_lon:             r3(snapPt[0]),
        snap_lat:             r3(snapPt[1]),
        arc_offset_m:         r1(arcOffsetM),
        window_geometry:      windowGeom,
      });
    }

    default:
      return JSON.stringify({ error: `Tool "${name}" is not yet implemented in this version.` });
  }
}
