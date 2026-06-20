import React, { useRef } from 'react';
import { useStore } from '../store.js';
import { useDraggable } from '../hooks.js';
import { diagnoseFailure } from '../diagnosis.js';

const FOW_NAMES = ['Undef', 'Motorway', 'Dual C/W', 'Single C/W', 'Roundabout', 'Traffic Sq', 'Slip Rd', 'Other'];
const FRC_NAMES = ['FRC0', 'FRC1', 'FRC2', 'FRC3', 'FRC4', 'FRC5', 'FRC6', 'FRC7'];

export default function ResultPanel() {
  const { decodeResult, clearResult, highlightedSegment, setHighlightedSegment,
          showTrace, toggleTrace, debugDecode, params } = useStore();
  const panelRef = useRef(null);
  const { pos, onMouseDown } = useDraggable(panelRef);

  if (!decodeResult) return null;

  const diagnosis = decodeResult.ok ? null : diagnoseFailure(decodeResult);

  // What the debug button should do depends on how much trace data we already have.
  const hasTrace = !!decodeResult.trace;
  const isFull   = params.trace_level === 'Full';
  const debugLabel = !hasTrace  ? 'Re-decode with tracing'
                   : !isFull    ? 'Re-decode with full trace'
                   : !showTrace ? 'Open trace panel'
                   : null; // full trace + panel open = nothing more to offer
  const debugAction = (!hasTrace || !isFull) ? debugDecode : toggleTrace;

  const panelStyle = pos
    ? { left: pos.left, top: pos.top, right: 'auto' }
    : (showTrace ? { right: '476px' } : undefined);

  return (
    <div ref={panelRef} className="result-panel" style={panelStyle}>
      <div
        className={`result-header ${decodeResult.ok ? 'ok' : 'err'} draggable-header`}
        onMouseDown={onMouseDown}
      >
        <span>{decodeResult.ok ? '✓ Decoded' : '✗ Failed'}</span>
        <button className="seg-info-close" onClick={clearResult}>✕</button>
      </div>
      <div className="result-body">
        {decodeResult.ok ? (
          <>
            <div className="result-meta">
              {decodeResult.segments.length} segment{decodeResult.segments.length !== 1 ? 's' : ''}
              {decodeResult.pos_offset_m > 0 && ` · +${decodeResult.pos_offset_m.toFixed(1)} m`}
              {decodeResult.neg_offset_m > 0 && ` · −${decodeResult.neg_offset_m.toFixed(1)} m`}
              {decodeResult.trace && !showTrace && (
                <button className="result-trace-link" onClick={toggleTrace} title="Open decode trace panel">
                  ⚡ Trace
                </button>
              )}
            </div>
            <div className="seg-table-wrap">
              <table className="seg-table">
                <thead>
                  <tr>
                    <th>Seg ID</th>
                    <th>FRC</th>
                    <th>FOW</th>
                    <th>OSM Way</th>
                  </tr>
                </thead>
                <tbody>
                  {decodeResult.segments.map((s, i) => {
                    const isActive = highlightedSegment?.tile === s.tile &&
                                     highlightedSegment?.local_index === s.local_index;
                    return (
                      <tr key={i} className={isActive ? 'seg-row-active' : ''}>
                        <td>
                          <button
                            className="seg-row-btn"
                            title={`Tile ${s.tile} · local index ${s.local_index}`}
                            onClick={() => setHighlightedSegment(
                              isActive ? null : { tile: s.tile, local_index: s.local_index }
                            )}
                          >{s.segment_id ?? i + 1}</button>
                        </td>
                        <td>{FRC_NAMES[s.frc] ?? s.frc}</td>
                        <td>{FOW_NAMES[s.fow] ?? s.fow}</td>
                        <td>
                          {s.osm_way_id != null
                            ? <a href={`https://www.openstreetmap.org/way/${s.osm_way_id}`} target="_blank" rel="noreferrer">{s.osm_way_id}</a>
                            : '—'}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </>
        ) : (
          <div className="result-failure">
            <div className="result-error">{decodeResult.error}</div>
            {diagnosis && (
              <div className="diag-body">
                <div className="diag-headline">{diagnosis.headline}</div>
                {diagnosis.bullets.length > 0 && (
                  <ul className="diag-bullets">
                    {diagnosis.bullets.map((b, i) => <li key={i}>{b}</li>)}
                  </ul>
                )}
                {diagnosis.suggestions.length > 0 && (
                  <div className="diag-suggestions">
                    <span className="diag-try-label">Try:</span>
                    <ul className="diag-bullets">
                      {diagnosis.suggestions.map((s, i) => <li key={i}>{s}</li>)}
                    </ul>
                  </div>
                )}
              </div>
            )}
            {debugLabel && (
              <button className="diag-debug-btn" onClick={debugAction}>
                {debugLabel}
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
