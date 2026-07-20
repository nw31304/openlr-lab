import React, { useState, useRef, useEffect } from 'react';
import { useStore, PRESETS } from '../store.js';

const TRACE_LEVELS = ['Off', 'Summary', 'Full'];

// The 9 OpenLR location types. Only Line and PointAlongLine have an encoder
// implemented so far; the rest are listed for discoverability.
const LOCATION_TYPES = [
  { id: 'Line',               label: 'Line',                    enabled: true },
  { id: 'PointAlongLine',     label: 'Point Along Line',        enabled: true },
  { id: 'ClosedLine',         label: 'Closed Line',             enabled: false },
  { id: 'PoiWithAccessPoint', label: 'POI with Access Point',   enabled: false },
  { id: 'GeoCoordinate',      label: 'Geo Coordinate',          enabled: false },
  { id: 'Circle',             label: 'Circle',                  enabled: false },
  { id: 'Rectangle',          label: 'Rectangle',               enabled: false },
  { id: 'Grid',               label: 'Grid',                    enabled: false },
  { id: 'Polygon',            label: 'Polygon',                 enabled: false },
];

export default function MenuBar() {
  const {
    showSegmentLayer, toggleSegmentLayer,
    showTrace, toggleTrace,
    showReplay, toggleReplay,
    showResult, toggleResult, decodeResult,
    toggleParams, toggleLlmSettings,
    llmConfig, llmChatOpen, toggleLlmChat,
    params, setTraceLevel,
    tileUrl, setTileUrl,
    decoding,
    mode, setMode, locationType, setLocationType,
  } = useStore();

  const [showTileMenu,  setShowTileMenu]  = useState(false);
  const [showTraceMenu, setShowTraceMenu] = useState(false);
  const [showLocTypeMenu, setShowLocTypeMenu] = useState(false);
  const [urlDraft, setUrlDraft]           = useState('');
  const tileMenuRef   = useRef(null);
  const traceMenuRef  = useRef(null);
  const locTypeMenuRef = useRef(null);
  const traceLevel   = params?.trace_level ?? 'Summary';

  // Sync urlDraft with the active tile URL whenever the menu opens.
  useEffect(() => {
    if (showTileMenu) setUrlDraft(tileUrl || import.meta.env.VITE_TILE_BASE_URL || 'http://localhost:5176');
  }, [showTileMenu, tileUrl]);

  // Close tile menu on outside click.
  useEffect(() => {
    if (!showTileMenu) return;
    const handler = (e) => {
      if (tileMenuRef.current && !tileMenuRef.current.contains(e.target))
        setShowTileMenu(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showTileMenu]);

  // Close trace menu on outside click.
  useEffect(() => {
    if (!showTraceMenu) return;
    const handler = (e) => {
      if (traceMenuRef.current && !traceMenuRef.current.contains(e.target))
        setShowTraceMenu(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showTraceMenu]);

  // Close location-type menu on outside click.
  useEffect(() => {
    if (!showLocTypeMenu) return;
    const handler = (e) => {
      if (locTypeMenuRef.current && !locTypeMenuRef.current.contains(e.target))
        setShowLocTypeMenu(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showLocTypeMenu]);

  function applyTileUrl() {
    const trimmed = urlDraft.trim();
    if (!trimmed) return;
    setTileUrl(trimmed);
    window.location.assign(window.location.pathname);
  }

  return (
    <div className="menu-bar">
      <span className="menu-title">OpenLRLab</span>

      <div className="mode-toggle">
        <button
          className={`mode-toggle-btn${mode !== 'encode' ? ' active' : ''}`}
          onClick={() => setMode('decode')}
          title="Decode mode"
        >Decode</button>

        {/* Encode is a dropdown-trigger, not a plain toggle: picking a
            location type both selects it and switches to encode mode in one
            click, and clicking it again while already in encode mode reopens
            the dropdown to switch types without leaving encode mode. */}
        <div className="menu-tile-wrap" ref={locTypeMenuRef}>
          <button
            className={`mode-toggle-btn${mode === 'encode' ? ' active' : ''}`}
            onClick={() => setShowLocTypeMenu(v => !v)}
            title="Encode mode — choose a location type"
          >Encode ▾ {LOCATION_TYPES.find(t => t.id === locationType)?.label ?? locationType}</button>

          {showLocTypeMenu && (
            <div className="menu-tile-dropdown">
              <div className="menu-tile-label">Location type</div>
              {LOCATION_TYPES.map(t => (
                <button
                  key={t.id}
                  className={`menu-trace-opt${locationType === t.id ? ' active' : ''}${!t.enabled ? ' disabled' : ''}`}
                  disabled={!t.enabled}
                  title={t.enabled ? '' : 'Not implemented yet'}
                  onClick={() => { setLocationType(t.id); setMode('encode'); setShowLocTypeMenu(false); }}
                >{t.label}</button>
              ))}
            </div>
          )}
        </div>
      </div>

      <div className="menu-divider" />

      <button
        className={`menu-btn${showSegmentLayer ? ' active' : ''}`}
        onClick={toggleSegmentLayer}
        title="Toggle road segment layer"
      >Segments</button>

      {mode !== 'encode' && (
        <>
          <button
            className={`menu-btn${showTrace ? ' active' : ''}`}
            onClick={toggleTrace}
            title="Toggle decode trace panel"
          >Trace</button>

          <button
            className={`menu-btn${showReplay ? ' active' : ''}`}
            onClick={toggleReplay}
            title="Toggle step replay bar"
          >Replay</button>

          {decodeResult && (
            <button
              className={`menu-btn${showResult ? ' active' : ''}`}
              onClick={toggleResult}
              title="Toggle results panel"
            >
              Results
              {decodeResult.ok
                ? <span className="menu-result-badge menu-result-ok">{decodeResult.segments?.length ?? '✓'}</span>
                : <span className="menu-result-badge menu-result-fail">✗</span>}
            </button>
          )}
        </>
      )}

      <div className="menu-spacer" />

      <button className="menu-btn" onClick={toggleParams} title="Decode parameters">
        Parameters
      </button>

      {/* Trace level dropdown */}
      <div className="menu-tile-wrap" ref={traceMenuRef}>
        <button
          className={`menu-btn${showTraceMenu ? ' active' : ''}`}
          onClick={() => setShowTraceMenu(v => !v)}
          title="Trace detail level"
        >Trace Level</button>

        {showTraceMenu && (
          <div className="menu-tile-dropdown menu-trace-dropdown">
            <div className="menu-tile-label">Trace detail level</div>
            {TRACE_LEVELS.map(lvl => (
              <button
                key={lvl}
                className={`menu-trace-opt${traceLevel === lvl ? ' active' : ''}`}
                onClick={() => { setTraceLevel(lvl); setShowTraceMenu(false); }}
              >{lvl}</button>
            ))}
          </div>
        )}
      </div>

      {llmConfig && (
        <button
          className={`menu-btn${llmChatOpen ? ' active' : ''}`}
          onClick={toggleLlmChat}
          title="AI chat"
        >AI Chat</button>
      )}

      <button
        className={`menu-btn${llmConfig ? ' configured' : ''}`}
        onClick={toggleLlmSettings}
        title="AI / LLM settings"
      >AI{llmConfig ? ' ●' : ''}</button>

      {/* Tile source dropdown */}
      <div className="menu-tile-wrap" ref={tileMenuRef}>
        <button
          className={`menu-btn${showTileMenu ? ' active' : ''}`}
          onClick={() => setShowTileMenu(v => !v)}
          title="Tile source"
        >Tile source</button>

        {showTileMenu && (
          <div className="menu-tile-dropdown">
            <div className="menu-tile-label">Tile server URL</div>
            <div className="menu-tile-row">
              <input
                className="menu-tile-input"
                type="url"
                value={urlDraft}
                onChange={e => setUrlDraft(e.target.value)}
                onKeyDown={e => e.key === 'Enter' && applyTileUrl()}
                spellCheck={false}
                placeholder="http://localhost:5176"
              />
            </div>
            <button
              className="menu-tile-apply"
              onClick={applyTileUrl}
              disabled={!urlDraft.trim()}
            >Apply &amp; reload</button>
          </div>
        )}
      </div>
    </div>
  );
}
