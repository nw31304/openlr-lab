import React, { useRef } from 'react';
import { useStore } from '../store.js';
import { useDraggable } from '../hooks.js';
import { diagnoseFailure, diagnoseSuccess } from '../diagnosis.js';
import { renderLlmText } from '../renderLlmText.jsx';

const FOW_NAMES = ['Undef', 'Motorway', 'Dual C/W', 'Single C/W', 'Roundabout', 'Traffic Sq', 'Slip Rd', 'Other'];
const FRC_NAMES = ['FRC0', 'FRC1', 'FRC2', 'FRC3', 'FRC4', 'FRC5', 'FRC6', 'FRC7'];

export default function ResultPanel() {
  const { decodeResult, showResult, hideResult, highlightedSegment, setHighlightedSegment,
          requestInfoSegment, showTrace, toggleTrace, debugDecode, params,
          llmConfig, llmChatOpen, toggleLlmChat, toggleLlmSettings } = useStore();
  const panelRef = useRef(null);
  const { pos, onMouseDown } = useDraggable(panelRef);

  if (!decodeResult || !showResult) return null;

  const diagnosis        = decodeResult.ok ? null : diagnoseFailure(decodeResult);
  const successWarning   = decodeResult.ok ? diagnoseSuccess(decodeResult) : null;

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
        <span>{decodeResult.ok
          ? (decodeResult.location_type === 'PointAlongLine' ? '✓ Decoded (Point)' : '✓ Decoded')
          : '✗ Failed'}</span>
        <button className="seg-info-close" onClick={hideResult}>✕</button>
      </div>
      <div className="result-body">
        {decodeResult.ok ? (
          <>
            <div className="result-meta">
              {decodeResult.location_type === 'PointAlongLine'
                ? 'PointAlongLine'
                : `${decodeResult.segments.length} segment${decodeResult.segments.length !== 1 ? 's' : ''}`}
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
                    <th>Segment Key</th>
                    <th>FRC</th>
                    <th>FOW</th>
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
                            title={`Tile ${s.tile} · tile index ${s.local_index} · internal ID ${s.segment_id}`}
                            onClick={() => {
                              const nowActive = !isActive;
                              setHighlightedSegment(nowActive ? { tile: s.tile, local_index: s.local_index } : null);
                              if (nowActive) requestInfoSegment(s.tile, s.local_index);
                            }}
                          >{s.source_id ?? s.segment_id ?? i + 1}</button>
                        </td>
                        <td>{FRC_NAMES[s.frc] ?? s.frc}</td>
                        <td>{FOW_NAMES[s.fow] ?? s.fow}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
            {decodeResult.location_type === 'PointAlongLine' && decodeResult.point_lon != null && (
              <div className="pal-point-info">
                <div className="pal-point-row">
                  <span className="pal-label">Point</span>
                  <span className="pal-value">{decodeResult.point_lat?.toFixed(6)}, {decodeResult.point_lon?.toFixed(6)}</span>
                </div>
                {decodeResult.orientation && decodeResult.orientation !== 'NoOrientation' && (
                  <div className="pal-point-row">
                    <span className="pal-label">Orientation</span>
                    <span className="pal-value">{decodeResult.orientation.replace(/([A-Z])/g, ' $1').trim()}</span>
                  </div>
                )}
                {decodeResult.side_of_road && decodeResult.side_of_road !== 'DirectlyOnOrNA' && (
                  <div className="pal-point-row">
                    <span className="pal-label">Side of road</span>
                    <span className="pal-value">{decodeResult.side_of_road}</span>
                  </div>
                )}
              </div>
            )}
            {successWarning && (
              <div className="diag-body diag-body-warn">
                <div className="diag-headline diag-headline-warn">⚠ {successWarning.headline}</div>
                <ul className="diag-bullets">
                  {successWarning.bullets.map((b, i) => <li key={i}>{b}</li>)}
                </ul>
                {successWarning.suggestions.length > 0 && (
                  <div className="diag-suggestions">
                    <span className="diag-try-label">Note:</span>
                    <ul className="diag-bullets">
                      {successWarning.suggestions.map((s, i) => <li key={i}>{s}</li>)}
                    </ul>
                  </div>
                )}
              </div>
            )}
            <button
              className="diag-debug-btn llm-ask-btn"
              onClick={llmConfig ? toggleLlmChat : toggleLlmSettings}
              title={llmConfig ? undefined : 'Configure an AI model to use this feature'}
            >
              {llmConfig ? (llmChatOpen ? '✦ Close AI Chat' : '✦ AI Chat') : '✦ AI Chat — configure…'}
            </button>
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
            <button
              className="diag-debug-btn llm-ask-btn"
              onClick={llmConfig ? toggleLlmChat : toggleLlmSettings}
              title={llmConfig ? undefined : 'Configure an AI model to use this feature'}
            >
              {llmConfig ? (llmChatOpen ? '✦ Close AI Chat' : '✦ AI Chat') : '✦ AI Chat — configure…'}
            </button>
          </div>
        )}

      </div>
    </div>
  );
}
