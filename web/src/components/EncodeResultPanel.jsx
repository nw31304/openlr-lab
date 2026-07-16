import React, { useState, useMemo } from 'react';
import { useStore, getEncoderSegment } from '../store.js';

const ORIENTATIONS = ['NoOrientation', 'FirstTowardSecond', 'SecondTowardFirst', 'BothDirections'];
const SIDES_OF_ROAD = ['DirectlyOnOrNA', 'Right', 'Left', 'Both'];
const FRC_NAMES = ['FRC0', 'FRC1', 'FRC2', 'FRC3', 'FRC4', 'FRC5', 'FRC6', 'FRC7'];
const FOW_NAMES = ['Undef', 'Motorway', 'Dual C/W', 'Single C/W', 'Roundabout', 'Traffic Sq', 'Slip Rd', 'Other'];

// Per-segment traversal direction (Fwd/Rev — the direction *this route*
// walks the segment, not the segment's own one-way/both-ways attribute),
// derived from shared node IDs between consecutive segments — the same
// convention TracePanel's candidate tables use ("Fwd"/"Bwd" per traversal).
// Segment IDs alone don't say which way a route walks them; comparing
// start_node/end_node against the next segment's endpoints does.
function computeTraversalDirs(segInfos) {
  const n = segInfos.length;
  const dirs = new Array(n).fill('Fwd');
  if (n < 2) return dirs;
  const s0 = segInfos[0], s1 = segInfos[1];
  const firstIsForward = s1.start_node === s0.end_node || s1.end_node === s0.end_node;
  dirs[0] = firstIsForward ? 'Fwd' : 'Rev';
  let exitNode = firstIsForward ? s0.end_node : s0.start_node;
  for (let i = 1; i < n; i++) {
    const s = segInfos[i];
    if (s.start_node === exitNode) { dirs[i] = 'Fwd'; exitNode = s.end_node; }
    else if (s.end_node === exitNode) { dirs[i] = 'Rev'; exitNode = s.start_node; }
    else { dirs[i] = 'Fwd'; exitNode = s.end_node; } // disconnected — shouldn't happen for a routed path
  }
  return dirs;
}

// Parses "lon,lat" pairs, one per line, ignoring blank lines.
function parseWaypointsText(text) {
  const pts = [];
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (!t) continue;
    const parts = t.split(',').map(s => parseFloat(s.trim()));
    if (parts.length !== 2 || parts.some(Number.isNaN)) {
      throw new Error(`invalid line "${line}" — expected "lon,lat"`);
    }
    pts.push({ lon: parts[0], lat: parts[1] });
  }
  return pts;
}

function CopyField({ label, value, onDecode }) {
  const [copied, setCopied] = useState(false);
  if (!value) return null;
  return (
    <div className="encode-output-row">
      <span className="encode-output-label">{label}</span>
      <input
        className={`encode-output-value${onDecode ? ' decodable' : ''}`}
        readOnly
        value={value}
        onFocus={e => e.target.select()}
        onClick={onDecode}
        title={onDecode ? `Click to decode this ${label} string` : undefined}
      />
      <button
        className="encode-output-copy"
        title={`Copy ${label}`}
        onClick={async () => {
          try { await navigator.clipboard.writeText(value); setCopied(true); setTimeout(() => setCopied(false), 1200); }
          catch { /* clipboard unavailable — user can still select+copy manually */ }
        }}
      >{copied ? '✓' : '⧉'}</button>
    </div>
  );
}

// Encode workflow controls, docked in the left side panel (Results) rather
// than a floating panel over the map — waypoints are drawn directly on the
// map itself (click/drag, entirely independent of this component); this is
// where you review them and actually perform the encode.
export default function EncodeResultPanel() {
  const {
    locationType,
    waypoints, setWaypoints, removeWaypoint, moveWaypointIndex, undo, clearWaypoints, waypointHistory,
    liveRoute, liveRouteError, liveRouteLoading,
    encoding, encodeResult, runEncode, runEncodePal,
    palOrientation, setPalOrientation, palSideOfRoad, setPalSideOfRoad,
    verifyResult, verifyToast, verifyReplaySteps,
    showTrace, toggleTrace, showReplay, toggleReplay,
    setOpenlrString, setMode, runDecode,
  } = useStore();

  // Jump to decode mode and decode this string, exactly as if it had been
  // pasted into the bottom bar — waypoints/liveRoute/encodeResult are
  // untouched by `setMode`, so switching back to Encode later just resumes
  // where you left off.
  const decodeThisString = (str) => {
    if (!str) return;
    setOpenlrString(str);
    setMode('decode');
    runDecode();
  };

  const [draft, setDraft] = useState('');
  const [draftError, setDraftError] = useState(null);

  const isPal = locationType === 'PointAlongLine';

  // Live per-segment breakdown of the drawn (not-yet-encoded) route, built
  // from the encoder's own loaded graph — no verify-decode needs to have
  // run yet. Skips rendering (rather than showing a partial/misleading
  // table) if any segment lookup fails, e.g. a tile not loaded yet.
  const liveSegRows = useMemo(() => {
    if (isPal || !liveRoute?.segments?.length) return null;
    const infos = liveRoute.segments.map(id => getEncoderSegment(id));
    if (infos.some(info => !info || info.error)) return null;
    const dirs = computeTraversalDirs(infos);
    return infos.map((info, i) => ({ ...info, travelDir: dirs[i] }));
  }, [isPal, liveRoute]);

  const maxFrc = liveSegRows?.length
    ? Math.max(...liveSegRows.map(s => s.frc))
    : null;

  function applyDraft() {
    try {
      const pts = parseWaypointsText(draft);
      setDraftError(null);
      setWaypoints(pts);
    } catch (e) {
      setDraftError(e.message);
    }
  }

  function doEncode() {
    if (isPal) runEncodePal();
    else runEncode();
  }

  const canEncode = isPal ? waypoints.length >= 1 : (waypoints.length >= 2 && !!liveRoute && !liveRouteError);

  return (
    <div className="result-panel">
      <div className="encode-result-title">✎ Encode — {isPal ? 'Point Along Line' : 'Line'}</div>

      <div className="encode-panel-body">
        <div className="encode-section-label">
          {isPal
            ? 'Right-click the map to place the point to encode'
            : 'Right-click the map to add a waypoint, insert one along the line, or move an existing one'}
        </div>

        {waypoints.length > 0 && (
          <div className="encode-waypoint-list">
            {waypoints.map((wp, i) => (
              <div key={i} className="encode-waypoint-row">
                <span className="encode-waypoint-index">{i + 1}</span>
                <span className="encode-waypoint-coord">{wp.lon.toFixed(5)}, {wp.lat.toFixed(5)}</span>
                {!isPal && (
                  <>
                    <button
                      className="encode-waypoint-btn"
                      onClick={() => moveWaypointIndex(i, -1)}
                      disabled={i === 0}
                      title="Move earlier in the route"
                    >▲</button>
                    <button
                      className="encode-waypoint-btn"
                      onClick={() => moveWaypointIndex(i, 1)}
                      disabled={i === waypoints.length - 1}
                      title="Move later in the route"
                    >▼</button>
                  </>
                )}
                <button
                  className="encode-waypoint-btn encode-waypoint-remove"
                  onClick={() => removeWaypoint(i)}
                  title="Remove this waypoint"
                >✕</button>
              </div>
            ))}
          </div>
        )}

        <div className="encode-section-label">Or paste "lon,lat" pairs below, one per line</div>
        <textarea
          className="encode-waypoints-input"
          rows={isPal ? 1 : 4}
          value={draft}
          onChange={e => setDraft(e.target.value)}
          placeholder={isPal ? '13.4050,52.5200' : '13.4050,52.5200\n13.4100,52.5230'}
          spellCheck={false}
        />
        {draftError && <div className="encode-error">{draftError}</div>}
        <div className="encode-btn-row">
          <button className="preset-btn" onClick={applyDraft} disabled={!draft.trim()}>Set Waypoints</button>
          <button className="preset-btn" onClick={undo} disabled={!waypointHistory.length}>Undo</button>
          <button className="preset-btn" onClick={clearWaypoints} disabled={!waypoints.length}>Clear</button>
        </div>

        {isPal ? (
          <div className="encode-status-line">
            {waypoints.length > 0 ? `point set: ${waypoints[0].lon.toFixed(5)}, ${waypoints[0].lat.toFixed(5)}` : 'no point set'}
          </div>
        ) : (
          <div className="encode-status-line">
            {waypoints.length} waypoint{waypoints.length === 1 ? '' : 's'}
            {liveRouteLoading && <span> · routing…</span>}
            {liveRoute && !liveRouteLoading && (
              <span> · {liveRoute.segments?.length ?? 0} segs, {liveRoute.length_m?.toFixed(0)} m</span>
            )}
          </div>
        )}
        {!isPal && liveRouteError && <div className="encode-error">{liveRouteError}</div>}

        {liveSegRows && (
          <>
            <div className="seg-table-wrap">
              <table className="seg-table">
                <thead>
                  <tr>
                    <th>Segment Key</th>
                    <th>FRC</th>
                    <th>FOW</th>
                    <th>Dir</th>
                    <th>Length</th>
                  </tr>
                </thead>
                <tbody>
                  {liveSegRows.map((s, i) => (
                    <tr key={i}>
                      <td title={`internal ID ${s.segment_id}`}>{s.stable_id ?? s.segment_id}</td>
                      <td>{FRC_NAMES[s.frc] ?? s.frc}</td>
                      <td>{FOW_NAMES[s.fow] ?? s.fow}</td>
                      <td title={s.travelDir === 'Fwd' ? 'Forward' : 'Reverse'}>{s.travelDir}</td>
                      <td>{s.length_m != null ? `${s.length_m} m` : '—'}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="encode-status-line">
              total {liveRoute.length_m?.toFixed(0)} m · lowest road class along route: {FRC_NAMES[maxFrc] ?? maxFrc}
            </div>
          </>
        )}

        {isPal && (
          <div className="encode-btn-row">
            <label className="encode-select-label">
              Orientation
              <select className="encode-select" value={palOrientation} onChange={e => setPalOrientation(e.target.value)}>
                {ORIENTATIONS.map(o => <option key={o} value={o}>{o}</option>)}
              </select>
            </label>
            <label className="encode-select-label">
              Side of road
              <select className="encode-select" value={palSideOfRoad} onChange={e => setPalSideOfRoad(e.target.value)}>
                {SIDES_OF_ROAD.map(s => <option key={s} value={s}>{s}</option>)}
              </select>
            </label>
          </div>
        )}

        <div className="encode-btn-row">
          <button className="preset-btn preset-btn-saved" onClick={doEncode} disabled={!canEncode || encoding}>
            {encoding ? 'Encoding…' : `Encode (${isPal ? 'PAL' : 'Line'})`}
          </button>
        </div>

        {encodeResult?.error && <div className="encode-error">{encodeResult.error}</div>}
        {encodeResult?.v3 && (
          <div className="encode-output-block">
            <CopyField label="v3"   value={encodeResult.v3}   onDecode={() => decodeThisString(encodeResult.v3)} />
            <CopyField label="TPEG" value={encodeResult.tpeg} onDecode={() => decodeThisString(encodeResult.tpeg)} />
          </div>
        )}

        {verifyResult && (
          <>
            <div className={`encode-verify-badge ${verifyResult.ok ? 'ok' : 'fail'}`}>
              {verifyResult.ok ? '✓ round-trip verified' : `⚠ verify failed: ${verifyToast?.message ?? verifyResult.error ?? ''}`}
            </div>
            <div className="encode-btn-row">
              <button className={`preset-btn${showTrace ? ' preset-btn-saved' : ''}`} onClick={toggleTrace}>Trace</button>
              <button
                className={`preset-btn${showReplay ? ' preset-btn-saved' : ''}`}
                onClick={toggleReplay}
                disabled={!verifyReplaySteps?.length}
                title={verifyReplaySteps?.length ? '' : 'No trace events to replay (raise the trace level)'}
              >Replay</button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
