import React, { useState, useRef, useEffect } from 'react';
import { useStore } from '../store.js';

const TRACE_LEVELS = ['Off', 'Summary', 'Full'];

export default function TopBar() {
  const { openlrString, showParams, showTrace, showResult, showSegmentLayer, decoding, params,
          decodeResult, tileUrl, setTileUrl, llmConfig,
          setOpenlrString, toggleParams, toggleTrace, toggleResult, toggleSegmentLayer,
          setTraceLevel, resetToDefaults, runDecode, toggleLlmSettings } = useStore();

  const [showGear, setShowGear] = useState(false);
  const gearRef = useRef(null);

  const traceLevel = params?.trace_level ?? 'Summary';
  // Dot on gear button when trace data exists but the panel is closed
  const hasTraceData = !!decodeResult?.trace && !showTrace;

  // Tile URL input — initialise from URL param if active, else from stored value
  const urlParam = new URLSearchParams(window.location.search).get('tiles') ?? '';
  const effectiveUrl = urlParam
    ? (urlParam.startsWith('http') ? urlParam : `http://localhost:5176/${urlParam}`)
    : (tileUrl || 'http://localhost:5176');
  const [urlDraft, setUrlDraft] = useState(effectiveUrl);

  const applyTileUrl = () => {
    const trimmed = urlDraft.trim();
    if (!trimmed) return;
    setTileUrl(trimmed);
    // Navigate to clean URL (strips ?tiles= param) so the stored value takes effect
    window.location.assign(window.location.pathname);
  };

  useEffect(() => {
    if (!showGear) return;
    const handler = (e) => {
      if (gearRef.current && !gearRef.current.contains(e.target)) setShowGear(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showGear]);

  return (
    <div className="top-bar">
      <input
        className="openlr-input"
        type="text"
        placeholder="Paste OpenLR string (v3 or TPEG, base64)…"
        value={openlrString}
        onChange={e => setOpenlrString(e.target.value)}
        onKeyDown={e => e.key === 'Enter' && runDecode()}
        spellCheck={false}
      />
      <div className="gear-wrap" ref={gearRef}>
        <button
          className={`params-btn${showGear ? ' active' : ''}${hasTraceData ? ' has-trace' : ''}`}
          onClick={() => setShowGear(g => !g)}
          title={hasTraceData ? 'Options (trace data available)' : 'Options'}
        >⚙</button>
        {showGear && (
          <div className="gear-panel">
            <div className="gear-row">
              <span>Road segments</span>
              <button className={`gear-toggle${showSegmentLayer ? ' on' : ''}`} onClick={() => { toggleSegmentLayer(); setShowGear(false); }}>
                {showSegmentLayer ? 'On' : 'Off'}
              </button>
            </div>
            <div className="gear-row">
              <span>Decode trace</span>
              <button className={`gear-toggle${showTrace ? ' on' : ''}`} onClick={() => { toggleTrace(); setShowGear(false); }}>
                {showTrace ? 'On' : 'Off'}
              </button>
            </div>
            <div className="gear-row">
              <span>Trace level</span>
              <div className="gear-level-group">
                {TRACE_LEVELS.map(lvl => (
                  <button
                    key={lvl}
                    className={`gear-level-btn${traceLevel === lvl ? ' active' : ''}`}
                    onClick={() => { setTraceLevel(lvl); setShowGear(false); }}
                  >{lvl}</button>
                ))}
              </div>
            </div>
            <div className="gear-divider" />
            <div className="gear-section-hdr">Tile source</div>
            {urlParam && (
              <div className="gear-url-note">
                URL param active — apply to persist
              </div>
            )}
            <div className="gear-url-row">
              <input
                className="gear-url-input"
                type="url"
                value={urlDraft}
                onChange={e => setUrlDraft(e.target.value)}
                onKeyDown={e => e.key === 'Enter' && applyTileUrl()}
                spellCheck={false}
                placeholder="http://localhost:5176"
              />
            </div>
            <button
              className="gear-url-apply"
              onClick={applyTileUrl}
              disabled={!urlDraft.trim()}
            >
              Apply &amp; reload
            </button>
            <div className="gear-divider" />
            <button className="gear-action" onClick={() => { toggleParams(); setShowGear(false); }}>
              Decoding Parameters…
            </button>
            <button className="gear-action" onClick={() => { toggleLlmSettings(); setShowGear(false); }}>
              AI settings…{llmConfig ? ' ●' : ''}
            </button>
            <button className="gear-action gear-reset" onClick={() => { resetToDefaults(); setShowGear(false); }}>
              Reset to defaults
            </button>
          </div>
        )}
      </div>
      {decodeResult && (
        <button
          className={`result-toggle-btn${showResult ? ' active' : ''}`}
          onClick={toggleResult}
          title={showResult ? 'Hide result panel' : 'Show result panel'}
        >{decodeResult.ok ? '✓' : '✗'}</button>
      )}
      <button className="decode-btn" onClick={runDecode} disabled={decoding}>
        {decoding ? '…' : 'Decode'}
      </button>
    </div>
  );
}
