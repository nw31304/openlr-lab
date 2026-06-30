import React from 'react';
import { useStore } from '../store.js';

export default function BottomBar() {
  const { openlrString, setOpenlrString, decoding, runDecode } = useStore();

  return (
    <div className="bottom-bar">
      <input
        className="openlr-input bottom-input"
        type="text"
        placeholder="Paste OpenLR string (v3 or TPEG, base64)…"
        value={openlrString}
        onChange={e => setOpenlrString(e.target.value)}
        onKeyDown={e => e.key === 'Enter' && runDecode()}
        spellCheck={false}
        autoComplete="off"
      />
      <button className="decode-btn" onClick={runDecode} disabled={decoding}>
        {decoding ? '…' : 'Decode'}
      </button>
    </div>
  );
}
