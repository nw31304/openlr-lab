// A shared decode link, either ?olr=<string> (current) or the legacy #q=
// hash format. Checked in two places that both need to agree on it: App.jsx
// gates the onboarding tour's auto-start on this (so a shared link takes the
// visitor straight to the decode instead of interrupting with a tour that
// would also stomp the decode's camera fit when dismissed — see the
// tourCameraRef effect in Map.jsx), and Map.jsx uses it to pre-fill and
// auto-run the decode itself.
export function getSharedOpenlrLink() {
  const q = new URLSearchParams(window.location.search).get('olr');
  if (q) return q;
  if (window.location.hash.startsWith('#q=')) {
    const legacy = decodeURIComponent(window.location.hash.slice(3));
    if (legacy) return legacy;
  }
  return null;
}

export function haversineM(lon1, lat1, lon2, lat2) {
  const R = 6371000;
  const φ1 = lat1 * Math.PI / 180, φ2 = lat2 * Math.PI / 180;
  const Δφ = (lat2 - lat1) * Math.PI / 180;
  const Δλ = (lon2 - lon1) * Math.PI / 180;
  const a = Math.sin(Δφ / 2) ** 2 + Math.cos(φ1) * Math.cos(φ2) * Math.sin(Δλ / 2) ** 2;
  return R * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
}


export function computeTraversalDirections(segments, cache) {
  const n = segments.length;
  if (n === 0) return [];
  const dirs = segments.map(s =>
    s.direction === 'Forward' ? 'Forward' : s.direction === 'Backward' ? 'Reverse' : null
  );
  const feats = segments.map(s => cache.get(s.segment_id));
  if (dirs[0] === null) {
    const f0 = feats[0], f1 = n > 1 ? feats[1] : null;
    if (f0 && f1) {
      const c0 = f0.geometry.coordinates, c1 = f1.geometry.coordinates;
      const dFF = haversineM(c0[c0.length-1][0], c0[c0.length-1][1], c1[0][0], c1[0][1]);
      const dFR = haversineM(c0[c0.length-1][0], c0[c0.length-1][1], c1[c1.length-1][0], c1[c1.length-1][1]);
      const dRF = haversineM(c0[0][0], c0[0][1], c1[0][0], c1[0][1]);
      const dRR = haversineM(c0[0][0], c0[0][1], c1[c1.length-1][0], c1[c1.length-1][1]);
      dirs[0] = Math.min(dFF, dFR) <= Math.min(dRF, dRR) ? 'Forward' : 'Reverse';
    } else {
      dirs[0] = 'Forward';
    }
  }
  let prevEnd = null;
  for (let i = 0; i < n; i++) {
    const ci = feats[i]?.geometry?.coordinates;
    if (!ci) { prevEnd = null; continue; }
    if (dirs[i] === null) {
      if (prevEnd) {
        const dFwd = haversineM(prevEnd[0], prevEnd[1], ci[0][0], ci[0][1]);
        const dRev = haversineM(prevEnd[0], prevEnd[1], ci[ci.length-1][0], ci[ci.length-1][1]);
        dirs[i] = dFwd <= dRev ? 'Forward' : 'Reverse';
      } else {
        dirs[i] = 'Forward';
      }
    }
    prevEnd = dirs[i] === 'Forward' ? ci[ci.length-1] : ci[0];
  }
  return dirs;
}

/**
 * Parse a WKT LINESTRING into a GeoJSON Feature.
 * Returns null if the WKT is missing or malformed.
 */
export function wktToGeoJSON(wkt) {
  const m = wkt?.match(/^LINESTRING \((.+)\)$/);
  if (!m) return null;
  const coordinates = m[1].split(',').map(p => {
    const [lon, lat] = p.trim().split(' ').map(Number);
    return [lon, lat];
  });
  return { type: 'Feature', geometry: { type: 'LineString', coordinates }, properties: {} };
}
