import React, { useRef, useState } from 'react';
import { useStore } from '../store.js';
import { useDraggable } from '../hooks.js';

const ORIENTATIONS = ['NoOrientation', 'FirstTowardSecond', 'SecondTowardFirst', 'BothDirections'];
const SIDES_OF_ROAD = ['DirectlyOnOrNA', 'Right', 'Left', 'Both'];

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

export default function EncodePanel() {
  const {
    mode, setMode,
    locationType,
    waypoints, setWaypoints, removeWaypoint, moveWaypointIndex, undo, clearWaypoints, waypointHistory,
    liveRoute, liveRouteError, liveRouteLoading,
    encoding, encodeResult, runEncode, runEncodePal,
    verifyResult, verifyToast, verifyReplaySteps,
    showResult, toggleResult, showTrace, toggleTrace, showReplay, toggleReplay,
    setOpenlrString, runDecode,
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
  const [orientation, setOrientation] = useState('NoOrientation');
  const [sideOfRoad, setSideOfRoad] = useState('DirectlyOnOrNA');
  const panelRef = useRef(null);
  const { pos, onMouseDown } = useDraggable(panelRef);

  if (mode !== 'encode') return null;

  const isPal = locationType === 'PointAlongLine';

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
    if (isPal) runEncodePal(orientation, sideOfRoad);
    else runEncode();
  }

  const panelStyle = pos ? { left: pos.left, top: pos.top, right: 'auto' } : undefined;
  const canEncode = isPal ? waypoints.length >= 1 : (waypoints.length >= 2 && !!liveRoute && !liveRouteError);

  return (
    <div ref={panelRef} className="encode-panel" style={panelStyle}>
      <div className="encode-panel-header draggable-header" onMouseDown={onMouseDown}>
        <span className="encode-panel-title">✎ Encode — {isPal ? 'Point Along Line' : 'Line'}</span>
        <button className="seg-info-close" onClick={() => setMode('decode')} title="Close (switch back to Decode)">✕</button>
      </div>

      <div className="encode-panel-body">
        <div className="encode-section-label">
          {isPal
            ? 'Click the map to place the point (first waypoint is used)'
            : 'Click the map to add waypoints, drag the line to insert one, or drag a marker to move it'}
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
            {waypoints.length > 1 && <span> (only the first waypoint is used)</span>}
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

        {isPal && (
          <div className="encode-btn-row">
            <label className="encode-select-label">
              Orientation
              <select className="encode-select" value={orientation} onChange={e => setOrientation(e.target.value)}>
                {ORIENTATIONS.map(o => <option key={o} value={o}>{o}</option>)}
              </select>
            </label>
            <label className="encode-select-label">
              Side of road
              <select className="encode-select" value={sideOfRoad} onChange={e => setSideOfRoad(e.target.value)}>
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
              <button className={`preset-btn${showResult ? ' preset-btn-saved' : ''}`} onClick={toggleResult}>Results</button>
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
