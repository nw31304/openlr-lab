import React, { useEffect, useState } from 'react';
import { PMTiles } from 'pmtiles';
import MapView from './components/Map.jsx';
import TopBar from './components/TopBar.jsx';
import ResultPanel from './components/ResultPanel.jsx';
import ParamsPanel from './components/ParamsPanel.jsx';
import TracePanel from './components/TracePanel.jsx';
import { setPmtiles, setDecoder, setZoom, useStore } from './store.js';
import { initWasm } from './wasm.js';

export default function App() {
  const [ready, setReady] = useState(false);
  const [error, setError] = useState(null);
  const [tilesBase, setTilesBase] = useState('/tiles');

  useEffect(() => {
    async function setup() {
      try {
        // ?tiles= URL param overrides the stored preference (useful for shareable
        // links and dev overrides). Falls back to the persisted tileUrl, then the
        // hardcoded dev default.
        const tilesParam = new URLSearchParams(window.location.search).get('tiles') ?? '';
        const storedUrl  = useStore.getState().tileUrl || 'http://localhost:5176';
        let base;
        if (tilesParam) {
          const isAbsolute = tilesParam.startsWith('http://') || tilesParam.startsWith('https://');
          base = isAbsolute ? tilesParam : `http://localhost:5176/${tilesParam}`;
        } else {
          base = storedUrl;
        }
        setTilesBase(base);

        console.log('[app] tile base:', base);
        const manifest = await fetch(`${base}/manifest.json`).then(r => r.json());
        const pmtiles = new PMTiles(`${base}/${manifest.archive}`);
        const decoder = await initWasm();

        setPmtiles(pmtiles);
        setDecoder(decoder);
        setZoom(manifest.tile_zoom ?? manifest.zoom ?? 12);
        setReady(true);
      } catch (e) {
        setError(e.message);
      }
    }
    setup();
  }, []);

  if (error) return (
    <div style={{position:'fixed',inset:0,display:'flex',alignItems:'center',justifyContent:'center',background:'#0a0a14',color:'#ff5566',fontFamily:'monospace',fontSize:14,padding:24,textAlign:'center'}}>
      ⚠ Failed to initialize:<br/>{error}
    </div>
  );

  return (
    <div className="app">
      <MapView tilesBase={tilesBase} ready={ready} />
      <TopBar />
      <ParamsPanel />
      <ResultPanel />
      <TracePanel />
    </div>
  );
}
