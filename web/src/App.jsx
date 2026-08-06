import React, { useEffect, useState } from 'react';
import { PMTiles } from 'pmtiles';
import MapView      from './components/Map.jsx';
import MenuBar      from './components/MenuBar.jsx';
import BottomBar    from './components/BottomBar.jsx';
import ReplayPanel  from './components/ReplayPanel.jsx';
import ResultPanel  from './components/ResultPanel.jsx';
import TracePanel   from './components/TracePanel.jsx';
import ParamsPanel  from './components/ParamsPanel.jsx';
import LlmSettingsPanel from './components/LlmSettingsPanel.jsx';
import LlmChatPanel     from './components/LlmChatPanel.jsx';
import DocumentationPage from './components/DocumentationPage.jsx';
import OnboardingTour   from './components/OnboardingTour.jsx';
import { setPmtiles, setDecoder, setEncoder, setZoom, useStore } from './store.js';
import DecodeToast from './components/DecodeToast.jsx';
import { initWasm } from './wasm.js';
import { getSharedOpenlrLink } from './utils.js';

export default function App() {
  const [ready, setReady]   = useState(false);
  const [error, setError]   = useState(null);
  const [tilesBase, setTilesBase] = useState('/tiles');
  const [urlDraft, setUrlDraft]   = useState('');

  const { showResult, toggleResult, showTrace, toggleTrace, showReplay, mode, replaySteps: decodeReplaySteps, verifyReplaySteps,
          hasSeenTour, tourStep, startTour } = useStore();
  const replaySteps = mode === 'encode' ? verifyReplaySteps : decodeReplaySteps;

  // First-ever load (per browser): auto-start the onboarding tour once the
  // app has actually finished loading, rather than over a blank/loading map.
  // Skipped entirely when a shared decode link (?olr=/#q=) is present — the
  // visitor's clear intent is to see that decode, not sit through a tour,
  // and the tour's own "snapshot camera at start, ease back to it at end"
  // behavior (Map.jsx's tourCameraRef effect) would otherwise capture the
  // pre-decode camera and stomp the decode's own camera fit the moment the
  // tour is dismissed.
  useEffect(() => {
    if (ready && !hasSeenTour && tourStep == null && !getSharedOpenlrLink()) startTour();
  }, [ready]); // eslint-disable-line react-hooks/exhaustive-deps

  // Keep `route` in sync with the browser back/forward buttons -- openDocs/
  // closeDocs (store.js) already pushState when navigating *within* the app,
  // this is only for history navigation the app itself didn't initiate.
  useEffect(() => {
    const onPopState = () => {
      useStore.setState({ route: window.location.pathname === '/docs' ? 'docs' : 'app' });
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  function resolveBase() {
    const defaultBase = import.meta.env.VITE_TILE_BASE_URL || 'http://localhost:5176';
    const tilesParam = new URLSearchParams(window.location.search).get('tiles') ?? '';
    const storedUrl  = useStore.getState().tileUrl || defaultBase;
    if (tilesParam) {
      const isAbsolute = tilesParam.startsWith('http://') || tilesParam.startsWith('https://');
      return isAbsolute ? tilesParam : `${defaultBase}/${tilesParam}`;
    }
    return storedUrl;
  }

  useEffect(() => {
    async function setup() {
      try {
        const base = resolveBase();
        setTilesBase(base);
        setUrlDraft(base);
        const manifest = await fetch(`${base}/manifest.json`).then(r => r.json());
        const pmtiles  = new PMTiles(`${base}/${manifest.archive}`);
        const [{ decoder, encoder, presetsJson }, header] = await Promise.all([
          initWasm(),
          pmtiles.getHeader().catch(() => null),
        ]);
        // The manifest is the sole source of truth for which zoom this archive
        // was actually tiled at (CLAUDE.md Invariant 11) -- every TileKey
        // computed downstream (prefetch, candidate search, A*) depends on it
        // matching exactly. A missing field here must be a hard error, not a
        // silent `?? 12` substitution: that would target the wrong z/x/y grid
        // for any archive actually built at a different zoom, with no
        // cross-check to ever catch the mismatch.
        const tileZoom = manifest.tile_zoom ?? manifest.zoom;
        if (!Number.isFinite(tileZoom)) {
          throw new Error(
            `manifest.json at ${base} has no tile_zoom field -- refusing to assume a default zoom, ` +
            `since a wrong zoom would silently make every tile request target the wrong grid cell for this archive.`
          );
        }
        setPmtiles(pmtiles);
        setDecoder(decoder);
        setEncoder(encoder);
        setZoom(tileZoom);
        useStore.getState().loadPresetsFromEngine(presetsJson);
        const { minLon, minLat, maxLon, maxLat } = header ?? {};
        const boundsValid = [minLon, minLat, maxLon, maxLat].every(Number.isFinite)
          && maxLon > minLon && maxLat > minLat;
        useStore.getState().setArchiveBounds(boundsValid ? [[minLon, minLat], [maxLon, maxLat]] : null);
        setReady(true);
      } catch (e) {
        setError(e.message);
        setUrlDraft(resolveBase());
      }
    }
    setup();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  function applyFixedUrl() {
    const trimmed = urlDraft.trim();
    if (!trimmed) return;
    useStore.getState().setTileUrl(trimmed);
    window.location.assign(window.location.pathname);
  }

  if (error) return (
    <div style={{position:'fixed',inset:0,display:'flex',flexDirection:'column',alignItems:'center',
      justifyContent:'center',gap:16,background:'#0a0a14',color:'#ff5566',
      fontFamily:'monospace',fontSize:14,padding:24,textAlign:'center'}}>
      <div>⚠ Failed to initialize: {error}</div>
      <div style={{color:'#aaa',fontSize:12}}>Check the tile server URL and press Apply to retry.</div>
      <div style={{display:'flex',gap:8,alignItems:'center'}}>
        <input
          style={{fontFamily:'monospace',fontSize:13,padding:'6px 10px',borderRadius:4,
            border:'1px solid #444',background:'#1a1a2e',color:'#eee',width:380}}
          value={urlDraft}
          onChange={e => setUrlDraft(e.target.value)}
          onKeyDown={e => e.key === 'Enter' && applyFixedUrl()}
          placeholder="http://localhost:5176"
        />
        <button
          style={{padding:'6px 14px',fontFamily:'monospace',fontSize:13,borderRadius:4,
            border:'none',background:'#3355cc',color:'#fff',cursor:'pointer'}}
          onClick={applyFixedUrl}
        >Apply</button>
      </div>
    </div>
  );

  return (
    <div className="app">
      <MenuBar />

      <div className="workspace">
        {/* ── Left panel ───────────────────────────────────── */}
        <div className={`side-panel side-panel-left${showResult ? ' open' : ' closed'}`}>
          <ResultPanel />
        </div>

        {/* ── Map area ─────────────────────────────────────── */}
        <div className="map-area">
          <MapView tilesBase={tilesBase} ready={ready} />

          {/* Panel edge tabs — always visible; arrow direction shows open/closed state */}
          <button
            className="panel-tab panel-tab-left"
            onClick={toggleResult}
            title={showResult ? 'Collapse results panel' : 'Open results panel'}
          >{showResult ? '◀' : '▶'}</button>
          <button
            className="panel-tab panel-tab-right"
            onClick={toggleTrace}
            title={showTrace ? 'Collapse trace panel' : 'Open trace panel'}
          >{showTrace ? '▶' : '◀'}</button>
        </div>

        {/* ── Right panel ──────────────────────────────────── */}
        <div className={`side-panel side-panel-right${showTrace ? ' open' : ' closed'}`}>
          <TracePanel />
        </div>
      </div>

      {/* ── Full-width replay timeline ──────────────────────────────── */}
      {showReplay && replaySteps?.length > 0 && <ReplayPanel />}

      {mode !== 'encode' && <BottomBar />}

      {/* ── Modals (remain floating over everything) ─────── */}
      <ParamsPanel />
      <LlmSettingsPanel />
      <LlmChatPanel />

      <DecodeToast />
      <OnboardingTour />

      {/* ── Documentation: a full-page overlay (route === 'docs'), not a modal --
          the rest of the app above stays mounted underneath (map/decoder state
          alive), so closing it returns to exactly where you left off. Rendered
          last so its z-index sits on top of everything else. ── */}
      <DocumentationPage />
    </div>
  );
}
