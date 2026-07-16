import React, { useState, useEffect, useRef } from 'react';
import { createPortal } from 'react-dom';
import { useStore } from '../store.js';

// ── Parameter docs ────────────────────────────────────────────────────────────

const PARAM_DOCS = {
  candidate_search_radius_m: {
    what: 'Maximum distance from each LRP coordinate to the nearest point on a road segment for that segment to be considered a candidate. Acts as the initial spatial filter before scoring.',
    increase: 'Recovers decodes where the map diverges significantly from the encoding map — the correct road is further away than expected. Cost: more candidates to score and route, slower per LRP.',
    decrease: 'Eliminates distant false candidates and speeds up decoding. Risk: fails if the encoding was made against a different map where roads are displaced by more than this radius.',
  },
  snap_to_endpoint_threshold_m: {
    what: 'If a projection point falls within this distance of a segment endpoint, it is snapped to that endpoint rather than treated as a mid-segment anchor. Avoids tiny degenerate partial edges.',
    increase: 'Cleaner graph traversal at junctions; fewer near-zero-length edge fragments. May introduce a small positional error if the correct anchor is genuinely close to but not at the endpoint.',
    decrease: 'More precise mid-segment projections. Risk: very small partial edges near junctions that can confuse the A* routing.',
  },
  distance_weight: {
    what: 'Multiplier on the positional distance component of the candidate score. Candidates physically closer to the LRP coordinate receive proportionally lower (better) scores.',
    increase: 'Strongly prefers segments closest to the LRP. Can reject valid matches if the encoding map and decoding map have slightly offset roads.',
    decrease: 'Distance matters less; bearing, FRC, and FOW carry proportionally more influence. Useful when map alignment is poor.',
  },
  bearing_weight: {
    what: 'Multiplier on the bearing penalty component of the candidate score. A candidate whose measured travel direction closely matches the encoded bearing is penalised less.',
    increase: 'Sharpens discrimination between parallel roads running in opposite directions. Can reject correct candidates on short segments where the measured bearing is noisy.',
    decrease: 'Direction fidelity matters less in ranking. Useful on very short segments or where map geometry makes bearing measurement unreliable.',
  },
  bearing_penalty_per_bucket: {
    what: 'Additional score penalty per 11.25° bearing bucket by which a candidate\'s measured bearing exceeds the encoded interval. Only meaningful for v3 binary references, which quantise bearings into 32 × 11.25° buckets.',
    increase: 'Sharper ranking penalty for each additional bucket of bearing deviation beyond the encoded window.',
    decrease: 'Softer per-bucket penalty; bearing deviations beyond the window are less costly in the score, so distant-bearing candidates can still rank well on other terms.',
  },
  max_bearing_deviation_deg: {
    what: 'Hard gate (τ, tau): candidates whose measured bearing differs from the encoded value by more than this many degrees are rejected outright. This is the map-divergence tolerance — it widens the encoded bearing interval symmetrically. For TPEG references (where the encoded interval is a point), τ is the entire acceptance window.',
    increase: 'Essential for TPEG references and for decoding against maps with different geometry. Admits candidates on roads whose measured bearing genuinely differs due to projection differences. Risk: may admit parallel roads running in similar directions.',
    decrease: 'Stricter — only candidates with near-exact bearing match proceed. Improves selectivity on dense road networks but fails on map-divergent references.',
  },
  frc_weight: {
    what: 'Multiplier on the FRC (Functional Road Class) penalty. The penalty reflects how many FRC classes apart the encoded FRC and the candidate segment\'s FRC are. FRC 0 = motorway, FRC 7 = minor/other.',
    increase: 'Strongly prefers segments of the correct road class. Can reject valid matches when the encoding map and decoding map classify the same road differently.',
    decrease: 'FRC mismatches are tolerated in ranking; useful when map providers use different classification conventions for the same road.',
  },
  fow_weight: {
    what: 'Multiplier on the FOW (Form of Way) penalty — how differently the candidate\'s road form (motorway, roundabout, slip road, etc.) compares to the encoded form.',
    increase: 'More discriminating about road form. Helps distinguish motorway mainlines from slip roads at complex junctions.',
    decrease: 'FOW differences are softened; useful when encoding and decoding maps disagree on form (e.g. one treats a loop as a roundabout, the other as a junction).',
  },
  interior_weight: {
    what: 'Extra penalty when the best projection point is at an interior location of a segment (not snapped to an endpoint). Slightly discourages mid-segment anchors in favour of endpoint matches.',
    increase: 'Strongly prefers endpoint candidates; may force suboptimal matching when the reference genuinely points to a mid-segment location.',
    decrease: 'Interior projections are ranked nearly equally with endpoint matches; appropriate for long segments with few intermediate junctions.',
  },
  wrong_endpoint_weight: {
    what: 'Penalty when the chosen traversal direction implies starting from the segment\'s far endpoint — i.e. the projection is near the end node for a forward traversal, which is topologically backwards.',
    increase: 'Stronger preference for clean start-from-start matches at junctions.',
    decrease: 'Allows more flexible endpoint traversal; can recover edge cases near junctions where the LRP sits close to a node shared between multiple segments.',
  },
  max_candidate_score: {
    what: 'Hard gate: candidates whose combined score (distance + bearing + FRC + FOW penalties) exceeds this value are rejected and never passed to the A* routing stage. Acts as a combined quality threshold.',
    increase: 'Admits lower-quality candidates; improves recall on divergent maps. More candidates means more A* invocations and higher routing cost.',
    decrease: 'Only high-confidence candidates advance to routing; faster A* but risks rejecting the only valid match when the map differs from the encoding.',
  },
  max_candidates_per_lrp: {
    what: 'Maximum number of accepted candidates passed to the A* routing stage per LRP. Candidates are ranked by score; only the top N proceed.',
    increase: 'More routing paths are explored. Recovers cases where the correct segment ranks outside the top few due to scoring imprecision near junctions.',
    decrease: 'Faster routing; only the best-scoring candidates are tried. Risk: misses the true path if the correct segment ranks outside the top N.',
  },
  dnp_tolerance_pct: {
    what: 'Fractional tolerance on the Distance to Next Point (DNP) validation. The routed path length must fall within DNP ± max(v3_bucket_half, pct × DNP). For longer legs, this tolerance scales up proportionally.',
    increase: 'Accepts paths whose measured length deviates further from the encoded DNP. Useful when road lengths in the decoding map differ from the encoding map.',
    decrease: 'Strict length matching. Rejects paths that are otherwise correct but measured slightly differently due to geometry differences between maps.',
  },
  max_path_search_factor: {
    what: 'Limits A* to paths no longer than DNP × factor. Prevents the search from exploring far-reaching detours that are implausible given the encoded distance.',
    increase: 'Needed when the route makes a significant detour (underpass, bridge, one-way system) that extends the true path length well beyond the straight-line estimate.',
    decrease: 'Prunes exploration earlier; faster but may reject valid indirect routes on constrained networks.',
  },
  max_astar_expansions: {
    what: 'Hard limit on the number of A* node expansions per routing leg. If reached, routing fails for that leg. A safety valve to prevent runaway searches on very large or poorly-connected graphs.',
    increase: 'More search effort; recovers difficult long legs or poorly-connected areas. Proportionally slower.',
    decrease: 'Fail faster on hard legs. Useful in batch mode where speed matters more than recall, or to surface graph connectivity issues quickly.',
  },
  max_routing_attempts: {
    what: 'Cap on the total number of candidate-combination trials across all legs. For N LRPs each with K candidates the full search space is Kᴺ⁻¹ combinations (tried in ascending score order). 0 = unlimited. When the cap fires, a RouteAttemptsExhausted event is emitted in the trace.',
    increase: 'Explores more combinations; recovers cases where the correct route only surfaces deeper in the ranked list. Proportionally slower on large references.',
    decrease: 'Stops earlier; faster but may miss the correct path when the best-scoring candidate pair is unroutable and the answer lies further down the list. Raise it if you see RouteAttemptsExhausted in the trace.',
  },
  lfrcnp_tolerance: {
    what: 'Relaxes the Lowest FRC to Next Point (LFRCNP) floor by this many FRC steps. Normally A* cannot traverse segments with FRC > LFRCNP. Each step of tolerance permits one additional road class — e.g. tolerance 1 allows LFRCNP+1 roads (slip roads, service connectors) on the route.',
    increase: 'The most common fix for A* failures where edges_skipped_frc in the trace is high. Allows lower-importance connector roads (ramps, service links) that were present in the encoding map to be used in routing. Risk: the route may use lower-class roads the encoder did not intend.',
    decrease: 'Enforces strict routing: only roads of the importance class the encoder intended may be used. Risk: A* fails on maps that classify connector roads at a different FRC than the encoding map.',
  },
  max_interior_turn_deviation_deg: {
    what: 'Hard gate on the interior A* path search: an edge is rejected if the geometric turn angle at its node deviates from straight-ahead by more than this many degrees (0° = dead ahead only, 180° = a full U-turn / doubling back). Independent of max_bearing_deviation_deg, which only scores candidates at the LRP endpoints and never constrains the interior route.',
    increase: 'Tolerates sharper turns along the route, up to and including near-U-turns at 180°. Needed for legitimate tight maneuvers (cul-de-sacs, service-road loops, some diamond interchange ramps) that this gate would otherwise block. Set to 180 to fully disable.',
    decrease: 'Rejects implausibly sharp turns (e.g. a route that doubles back on itself at a roundabout exit due to a missing OSM turn restriction). Risk: may block a legitimate route that genuinely requires a tight turn.',
  },
  max_leg_m: {
    what: 'Encoder-only: Rule-1 cap on the distance between consecutive LRPs. Not part of the OpenLR wire format itself — it only controls how the encoder chooses to split a location into legs. Clamped to the architecture\'s 15 km ceiling.',
    increase: 'Fewer intermediate LRPs for long routes. Rarely needed above the 15 km default.',
    decrease: 'Forces more frequent intermediate LRPs, keeping each leg small enough for a constrained decoder tile budget (e.g. a memory-limited head unit).',
  },
};

// ── Field definitions ─────────────────────────────────────────────────────────

const SCALAR_FIELDS = [
  { key: 'candidate_search_radius_m',    label: 'Search radius',          unit: 'm',     step: 10,    min: 10,    max: 500    },
  { key: 'snap_to_endpoint_threshold_m', label: 'Snap threshold',         unit: 'm',     step: 1,     min: 0,     max: 50     },
  { key: 'distance_weight',              label: 'Distance weight',         unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'bearing_weight',               label: 'Bearing weight',          unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'bearing_penalty_per_bucket',   label: 'Bearing pen./bucket',     unit: '',      step: 0.01,  min: 0,     max: 1      },
  { key: 'max_bearing_deviation_deg',    label: 'Max bearing deviation',   unit: '°',     step: 5,     min: 0,     max: 180    },
  { key: 'frc_weight',                   label: 'FRC weight',              unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'fow_weight',                   label: 'FOW weight',              unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'interior_weight',              label: 'Interior snap weight',    unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'wrong_endpoint_weight',        label: 'Wrong endpoint weight',   unit: '',      step: 0.5,   min: 0                  },
  { key: 'max_candidate_score',          label: 'Max candidate score',     unit: '',      step: 0.05,  min: 0,     max: 5      },
  { key: 'max_candidates_per_lrp',       label: 'Max candidates',          unit: '/LRP',  step: 1,     min: 1,     max: 50,    int: true },
  { key: 'dnp_tolerance_pct',            label: 'DNP tolerance',           unit: '',      step: 0.05,  min: 0,     max: 1      },
  { key: 'max_path_search_factor',       label: 'Path search factor',      unit: '×DNP',  step: 0.5,   min: 1,     max: 20     },
  { key: 'max_astar_expansions',         label: 'A* expansion cap',        unit: '',      step: 10000, min: 0,     max: 500000, int: true },
  { key: 'max_routing_attempts',         label: 'Routing attempt cap',     unit: '',      step: 1,     min: 0,     max: 500,    int: true },
  { key: 'lfrcnp_tolerance',             label: 'LFRCNP tolerance',        unit: 'steps', step: 1,     min: 0,     max: 7,     int: true },
  { key: 'max_interior_turn_deviation_deg', label: 'Max turn deviation',   unit: '°',     step: 5,     min: 0,     max: 180    },
];

// Fields whose value lives outside `params` (their own store slice, own
// setter) — not part of SCALAR_FIELDS' generic params[key]/setParam loop,
// but still documented/titled the same way via ParamDocPopout.
const EXTRA_FIELDS = {
  max_leg_m: { label: 'Max inter-LRP dist.', unit: 'm' },
};

const FRC_LABELS = ['FRC0', 'FRC1', 'FRC2', 'FRC3', 'FRC4', 'FRC5', 'FRC6', 'FRC7'];
const FOW_LABELS = ['Undef', 'Mway', 'MultiC', 'SingleC', 'Roundbt', 'TrafSq', 'Slip', 'Other'];

// ── Helpers ───────────────────────────────────────────────────────────────────

function decimalsForStep(step) {
  if (step >= 1) return 0;
  return Math.max(0, -Math.floor(Math.log10(step)));
}

function fmt(v, dec, isInt) {
  if (v == null || isNaN(v)) return '0';
  return isInt ? String(Math.round(v)) : String(parseFloat(v.toFixed(dec)));
}

// ── SpinInput ─────────────────────────────────────────────────────────────────

function SpinInput({ value, onChange, step, min, max, isInt, className, style }) {
  const dec = decimalsForStep(step);
  const [text, setText] = React.useState(() => fmt(value, dec, isInt));
  const textRef = React.useRef(text);

  React.useEffect(() => {
    const s = fmt(value, dec, isInt);
    textRef.current = s;
    setText(s);
  }, [value]); // eslint-disable-line react-hooks/exhaustive-deps

  const commit = (raw) => {
    const clamped = max != null ? Math.min(max, Math.max(min, raw)) : Math.max(min, raw);
    const out = isInt ? Math.round(clamped) : parseFloat(clamped.toFixed(dec));
    const s = fmt(out, dec, isInt);
    textRef.current = s;
    setText(s);
    onChange(out);
  };

  const adjust = (dir) => {
    const base = parseFloat(textRef.current);
    if (!isNaN(base)) commit(base + dir * step);
  };

  return (
    <div className={`spin-wrap ${className ?? ''}`} style={style}>
      <input
        className="spin-input"
        type="text"
        inputMode="numeric"
        value={text}
        onChange={e => { textRef.current = e.target.value; setText(e.target.value); }}
        onBlur={() => {
          const v = isInt ? parseInt(textRef.current, 10) : parseFloat(textRef.current);
          if (!isNaN(v)) commit(v);
          else { const s = fmt(value, dec, isInt); textRef.current = s; setText(s); }
        }}
        onKeyDown={e => {
          if (e.key === 'Enter') {
            const v = isInt ? parseInt(textRef.current, 10) : parseFloat(textRef.current);
            if (!isNaN(v)) commit(v);
          }
        }}
      />
      <div className="spin-arrows">
        <button type="button" className="spin-arrow" onClick={() => adjust(1)}>▲</button>
        <button type="button" className="spin-arrow" onClick={() => adjust(-1)}>▼</button>
      </div>
    </div>
  );
}

// ── Param doc popout (portal) ─────────────────────────────────────────────────

function ParamDocPopout({ docKey, pos, onClose }) {
  const ref = useRef(null);
  const doc = PARAM_DOCS[docKey];

  useEffect(() => {
    if (!docKey) return;
    function handler(e) {
      if (ref.current && !ref.current.contains(e.target)) onClose();
    }
    function keyHandler(e) {
      if (e.key === 'Escape') onClose();
    }
    document.addEventListener('mousedown', handler);
    document.addEventListener('keydown', keyHandler);
    return () => {
      document.removeEventListener('mousedown', handler);
      document.removeEventListener('keydown', keyHandler);
    };
  }, [docKey, onClose]);

  if (!docKey || !doc) return null;

  const field = SCALAR_FIELDS.find(f => f.key === docKey) ?? EXTRA_FIELDS[docKey];
  const title = field ? `${field.label}${field.unit ? ` (${field.unit})` : ''}` : docKey;

  const style = {
    position: 'fixed',
    top: pos.top,
    left: pos.left,
    zIndex: 200,
  };

  return createPortal(
    <div ref={ref} className="param-doc-popout" style={style}>
      <div className="param-doc-title">{title}</div>
      <div className="param-doc-section">
        <div className="param-doc-body">{doc.what}</div>
      </div>
      <div className="param-doc-section">
        <div className="param-doc-dir param-doc-dir-up">▲ Increasing</div>
        <div className="param-doc-body">{doc.increase}</div>
      </div>
      <div className="param-doc-section">
        <div className="param-doc-dir param-doc-dir-down">▼ Decreasing</div>
        <div className="param-doc-body">{doc.decrease}</div>
      </div>
      <button className="param-doc-close" onClick={onClose}>✕</button>
    </div>,
    document.body
  );
}

// ── PenaltyTable ──────────────────────────────────────────────────────────────

function PenaltyTable({ tableKey, rowLabels, colLabels }) {
  const { params, setTableCell } = useStore();
  const table = params[tableKey];
  if (!table) return null;

  return (
    <div className="penalty-table-wrap">
      <div className="pt-axis-col-label">Segment (map) →</div>
      <div className="pt-axis-row-wrap">
        <div className="pt-axis-row-label">LRP (ref) ↓</div>
        <div className="penalty-table-grid" style={{ gridTemplateColumns: `52px repeat(${colLabels.length}, 52px)` }}>
          <div className="pt-corner" />
          {colLabels.map(l => <div key={l} className="pt-col-label">{l}</div>)}
          {table.map((row, i) => (
            <React.Fragment key={i}>
              <div className="pt-row-label">{rowLabels[i]}</div>
              {row.map((val, j) => (
                <SpinInput
                  key={j}
                  className="pt-spin"
                  value={val}
                  step={0.05}
                  min={0}
                  max={1}
                  isInt={false}
                  onChange={v => setTableCell(tableKey, i, j, v)}
                  style={{ background: `rgba(220,80,80,${val * 0.7})` }}
                />
              ))}
            </React.Fragment>
          ))}
        </div>
      </div>
    </div>
  );
}

// ── ParamsPanel ───────────────────────────────────────────────────────────────

export default function ParamsPanel() {
  const {
    params, showParams, setParam, toggleParams,
    loadPreset, saveParamSet, deleteParamSet, loadParamSet, savedParamSets,
    resetToDefaults, maxEncodeLegM, setMaxEncodeLegM,
  } = useStore();
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [activeDoc, setActiveDoc] = useState(null);
  const [docPos, setDocPos] = useState({ top: 0, left: 0 });
  const [saving, setSaving] = useState(false);
  const [saveName, setSaveName] = useState('');

  if (!showParams) return null;

  function openDoc(key, e) {
    if (activeDoc === key) { setActiveDoc(null); return; }
    const rect = e.currentTarget.getBoundingClientRect();
    const popoutWidth = 300;
    const left = Math.max(8, Math.min(
      rect.left + rect.width / 2 - popoutWidth / 2,
      window.innerWidth - popoutWidth - 8
    ));
    const spaceBelow = window.innerHeight - rect.bottom - 8;
    const top = spaceBelow > 180 ? rect.bottom + 6 : rect.top - 6;
    setDocPos({ top, left });
    setActiveDoc(key);
  }

  return (
    <div className="params-panel">
      <div className="params-panel-header">
        <span className="params-panel-title">Decode Parameters</span>
        <button className="params-panel-close" onClick={toggleParams} title="Close">✕</button>
      </div>
      <div className="params-panel-body">

        {/* ── Presets bar ── */}
        <div className="presets-bar">
          <div className="presets-row">
            <span className="presets-label">Presets</span>
            {['Permissive', 'Default', 'Strict'].map(name => (
              <button key={name} className="preset-btn" onClick={() => loadPreset(name)}>{name}</button>
            ))}
          </div>

          {Object.keys(savedParamSets).length > 0 && (
            <div className="presets-row presets-row-saved">
              <span className="presets-label">Saved</span>
              {Object.keys(savedParamSets).map(name => (
                <span key={name} className="preset-saved-chip">
                  <button className="preset-btn preset-btn-saved" onClick={() => loadParamSet(name)}>{name}</button>
                  <button className="preset-chip-delete" onClick={() => deleteParamSet(name)} title={`Delete "${name}"`}>✕</button>
                </span>
              ))}
            </div>
          )}

          {saving ? (
            <div className="presets-row preset-save-row">
              <input
                className="preset-save-input"
                autoFocus
                placeholder="Name…"
                value={saveName}
                onChange={e => setSaveName(e.target.value)}
                onKeyDown={e => {
                  if (e.key === 'Enter' && saveName.trim()) {
                    saveParamSet(saveName.trim(), params);
                    setSaving(false); setSaveName('');
                  }
                  if (e.key === 'Escape') { setSaving(false); setSaveName(''); }
                }}
              />
              <button
                className="preset-save-confirm"
                disabled={!saveName.trim()}
                onClick={() => {
                  saveParamSet(saveName.trim(), params);
                  setSaving(false); setSaveName('');
                }}
              >Save</button>
              <button className="preset-save-cancel" onClick={() => { setSaving(false); setSaveName(''); }}>Cancel</button>
            </div>
          ) : (
            <button className="preset-save-link" onClick={() => setSaving(true)}>+ Save current as…</button>
          )}

          <button className="preset-save-link" onClick={resetToDefaults}>Reset decode params to defaults</button>
        </div>

        <div className="params-grid">
          {SCALAR_FIELDS.map(({ key, label, unit, step, min, max, int: isInt }) => (
            <label key={key} className="param-row">
              <span className="param-label-group">
                <span className="param-label">{label}{unit ? <em> {unit}</em> : ''}</span>
                <button
                  type="button"
                  className={`param-doc-btn${activeDoc === key ? ' active' : ''}`}
                  onClick={e => { e.preventDefault(); openDoc(key, e); }}
                  title="What does this parameter do?"
                >?</button>
              </span>
              <SpinInput
                value={params[key] ?? 0}
                step={step}
                min={min}
                max={max}
                isInt={isInt}
                onChange={v => setParam(key, v)}
              />
            </label>
          ))}

          <label className="param-row">
            <span className="param-label-group">
              <span className="param-label">Max inter-LRP dist.<em> m</em></span>
              <button
                type="button"
                className={`param-doc-btn${activeDoc === 'max_leg_m' ? ' active' : ''}`}
                onClick={e => { e.preventDefault(); openDoc('max_leg_m', e); }}
                title="What does this parameter do?"
              >?</button>
            </span>
            <SpinInput
              value={maxEncodeLegM}
              step={100}
              min={1}
              max={15000}
              isInt={true}
              onChange={setMaxEncodeLegM}
            />
          </label>
        </div>

        <button className="advanced-toggle" onClick={() => setShowAdvanced(s => !s)}>
          {showAdvanced ? '▾' : '▸'} Advanced penalty tables
        </button>

        {showAdvanced && (
          <>
            <div className="table-section">
              <div className="table-section-title">FRC penalty table</div>
              <PenaltyTable tableKey="frc_penalty_table" rowLabels={FRC_LABELS} colLabels={FRC_LABELS} />
            </div>

            <div className="table-section">
              <div className="table-section-title">FOW penalty table</div>
              <PenaltyTable tableKey="fow_penalty_table" rowLabels={FOW_LABELS} colLabels={FOW_LABELS} />
            </div>
          </>
        )}
      </div>

      <ParamDocPopout
        docKey={activeDoc}
        pos={docPos}
        onClose={() => setActiveDoc(null)}
      />
    </div>
  );
}
