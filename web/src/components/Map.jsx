import React, { useEffect, useRef, useState } from 'react';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
import { PMTiles } from 'pmtiles';
import { useStore, getSegmentId, getNodeId, getSegGeomCache, getSegIdToTile, getTileGeomCache, getSnapCandidates, loadEncoderTilesNear, previewRouteBetween } from '../store.js';
import { useDraggable } from '../hooks.js';
import { emptyState, applyStep, computeVisualState, stateToGeoJSON } from '../replayEngine.js';
import { haversineM } from '../utils.js';


// Inline SVG tip for speech bubbles — above: tip points down, below: tip points up.
// W/H are in px; tipLeft is the center of the tip within the popup (pixels from popup left).
function TipSvg({ placement, tipLeft }) {
  if (!placement || tipLeft == null) return null;
  const W = 24, H = 12;
  const left = tipLeft - W / 2;
  const fill   = 'rgba(20,20,36,0.97)';
  const stroke = 'rgba(255,255,255,0.18)';
  const base = { position: 'absolute', left, width: W, height: H, display: 'block', pointerEvents: 'none' };
  if (placement === 'above') {
    return (
      <svg style={{ ...base, bottom: -H }} viewBox={`0 0 ${W} ${H}`}>
        <polygon  points={`0,0 ${W/2},${H} ${W},0`}         fill={fill} />
        <polyline points={`0,0 ${W/2},${H} ${W},0`}         fill="none" stroke={stroke} strokeWidth="1" />
      </svg>
    );
  }
  return (
    <svg style={{ ...base, top: -H }} viewBox={`0 0 ${W} ${H}`}>
      <polygon  points={`0,${H} ${W/2},0 ${W},${H}`}        fill={fill} />
      <polyline points={`0,${H} ${W/2},0 ${W},${H}`}        fill="none" stroke={stroke} strokeWidth="1" />
    </svg>
  );
}

// Returns { style, placement, tipLeft } for a speech-bubble popup.
// For callout-above: pins popup.bottom = anchor.y − tipH via the CSS `bottom`
// property (relative to the map container height).  No height estimate needed —
// the popup simply grows upward from that pinned edge, so the SVG tip child at
// `bottom: -tipH` always lands exactly on anchor.y regardless of content height.
// For callout-below: sets top = anchor.y + tipH; popup grows downward.
function popupPlacement(anchor, w = 260, containerW = null, containerH = null) {
  if (!anchor) return { style: undefined, placement: null, tipLeft: w / 2 };
  const edge = 8, tipH = 12;
  const cw = containerW || window.innerWidth;
  const ch = containerH || window.innerHeight;

  // Centre popup on anchor, then clamp to stay inside the container.
  const rawLeft = anchor.x - w / 2;
  const left    = Math.max(edge, Math.min(rawLeft, cw - w - edge));

  // Place above when the anchor is in the lower half — more room to grow upward.
  const above = anchor.y > ch / 2;

  // tipLeft: horizontal center of the tip within the popup (clamped 12–w–12).
  const tipLeft = Math.max(12, Math.min(anchor.x - left, w - 12));

  if (above) {
    // `bottom: ch - anchor.y + tipH` → popup.bottom = anchor.y − tipH.
    // SVG tip child at `bottom: -tipH` → tip visual bottom = anchor.y. ✓
    return {
      style: { position: 'absolute', left, bottom: ch - anchor.y + tipH, top: 'auto', right: 'auto' },
      placement: 'above',
      tipLeft,
    };
  }
  return {
    style: { position: 'absolute', left, top: anchor.y + tipH, bottom: 'auto', right: 'auto' },
    placement: 'below',
    tipLeft,
  };
}
import { decodeTile } from '../tileDecoder.js';
import { diagnoseSegment } from '../diagnosis.js';

// ── Constants ──────────────────────────────────────────────────────────────────

const TILE_ZOOM = 12;
const MIN_LOAD_ZOOM = 10;

// ── Basemap definitions ────────────────────────────────────────────────────────

function rasterStyle(tiles, attribution, maxzoom = 19) {
  return {
    version: 8,
    sources: {
      basemap: { type: 'raster', tiles, tileSize: 256, attribution, maxzoom },
    },
    layers: [{ id: 'basemap', type: 'raster', source: 'basemap' }],
  };
}

const BASEMAPS = [
  { id: 'liberty',     label: 'Liberty',      style: 'https://tiles.openfreemap.org/styles/liberty' },
  { id: 'bright',      label: 'Bright',       style: 'https://tiles.openfreemap.org/styles/bright' },
  { id: 'positron',    label: 'Positron',     style: 'https://tiles.openfreemap.org/styles/positron' },
  { id: 'osm',         label: 'OSM',          style: rasterStyle(
    ['https://tile.openstreetmap.org/{z}/{x}/{y}.png'],
    '© <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors') },
  { id: 'carto-light', label: 'Carto Light',  style: rasterStyle(
    ['https://a.basemaps.cartocdn.com/light_all/{z}/{x}/{y}.png',
     'https://b.basemaps.cartocdn.com/light_all/{z}/{x}/{y}.png',
     'https://c.basemaps.cartocdn.com/light_all/{z}/{x}/{y}.png'],
    '© <a href="https://carto.com/attributions">CARTO</a> © <a href="https://www.openstreetmap.org/copyright">OSM</a> contributors') },
  { id: 'carto-dark',  label: 'Carto Dark',   style: rasterStyle(
    ['https://a.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png',
     'https://b.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png',
     'https://c.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png'],
    '© <a href="https://carto.com/attributions">CARTO</a> © <a href="https://www.openstreetmap.org/copyright">OSM</a> contributors') },
  { id: 'satellite',   label: 'Satellite',    style: rasterStyle(
    ['https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}'],
    'Tiles © Esri — Esri, Maxar, Earthstar Geographics') },
];

// Custom sources/layers to preserve across basemap switches via transformStyle.
const CUSTOM_SOURCES = new Set([
  'olr-segments', 'olr-nodes', 'decoded-path', 'lrp-markers',
  'lrp-snap', 'lrp-displacement',
  'offset-uncertainty', 'lrp-bearing', 'highlighted-segment', 'trace-segment',
  'replay-radius', 'replay-route', 'replay-traversed', 'replay-candidates', 'replay-cloud', 'replay-frontier', 'replay-leg', 'replay-flash',
  'measure-line', 'measure-points', 'pal-point', 'pal-pulse', 'pal-uncertainty', 'encode-route', 'encode-ghost', 'encode-offset-stubs',
  'encode-snap-candidates', 'encode-snap-candidate-active', 'encode-preview-route', 'encode-click-point',
]);
const CUSTOM_LAYER_IDS = new Set([
  'olr-frc0','olr-frc1','olr-frc2','olr-frc3','olr-frc4','olr-frc5','olr-frc6','olr-frc7',
  'olr-highlight', 'olr-nodes-circle', 'decoded-path-line', 'lrp-markers-circle',
  'lrp-displacement-line', 'lrp-displacement-arrow',
  'offset-uncertainty-line',
  'lrp-bearing-fill', 'lrp-bearing-outline',
  'highlighted-segment-halo', 'highlighted-segment-line',
  'trace-segment-halo', 'trace-segment-line',
  'replay-radius-fill', 'replay-radius-line',
  'replay-route-casing', 'replay-route-line',
  'replay-traversed-line',
  'replay-candidates-circle',
  'replay-cloud-circle',
  'replay-frontier-circle',
  'replay-leg-from', 'replay-leg-to',
  'replay-flash-ring',
  'measure-line-layer', 'measure-points-layer',
  'pal-point-layer', 'pal-pulse-ring', 'pal-uncertainty-line',
  'encode-route-casing', 'encode-route-line', 'encode-ghost-line', 'encode-offset-stubs-line',
  'encode-snap-candidates-circle', 'encode-snap-candidate-active-ring',
  'encode-preview-route-casing', 'encode-preview-route-line',
  'encode-click-point-circle',
]);

const FRC_COLOR = ['#e8002d', '#ff7700', '#e8c800', '#00aa44',
                   '#00aaff', '#0055ff', '#aa00ff', '#888888'];
const FRC_LABEL = ['0 · Motorway', '1 · Trunk/Primary', '2 · Secondary', '3 · Tertiary',
                   '4 · Unclassified', '5 · Residential', '6 · Svc/Living St', '7 · Other'];
const FRC_WIDTH = [4, 3, 2.5, 2, 1.5, 1.5, 1.2, 1];

// ── Slippy tile helpers ────────────────────────────────────────────────────────

function lngLatToTile(lng, lat, z) {
  const n = 2 ** z;
  const latRad = lat * Math.PI / 180;
  const x = Math.floor((lng + 180) / 360 * n);
  const y = Math.floor((1 - Math.log(Math.tan(latRad) + 1 / Math.cos(latRad)) / Math.PI) / 2 * n);
  return [Math.max(0, Math.min(n - 1, x)), Math.max(0, Math.min(n - 1, y))];
}

function tilesForBounds(bounds, z) {
  const [x0, y0] = lngLatToTile(bounds.getWest(),  bounds.getNorth(), z);
  const [x1, y1] = lngLatToTile(bounds.getEast(),  bounds.getSouth(), z);
  const tiles = [];
  for (let x = x0; x <= x1; x++)
    for (let y = y0; y <= y1; y++)
      tiles.push({ z, x, y });
  return tiles;
}

// ── LRP bearing helper ─────────────────────────────────────────────────────────

function formatBearing(lb, ub) {
  if (Math.abs(ub - lb) < 0.1) return `${lb.toFixed(1)}°`;
  return `${lb.toFixed(1)}° – ${ub.toFixed(1)}°`;
}

// 32-sector compass rose matching v3 bearing quantization (11.25° per sector).
// Active sectors (those inside [lb, ub]) are highlighted in magenta.
function BearingCompass({ lb, ub, size = 76 }) {
  const N = 32;
  const SECTOR = 360 / N;
  const cx = size / 2, cy = size / 2;
  const outerR = size / 2 - 4;
  const innerR = outerR * 0.42;

  function sectorActive(i) {
    const mid = ((i + 0.5) * SECTOR + 360) % 360;
    const lo = ((lb % 360) + 360) % 360;
    const hi = ((ub % 360) + 360) % 360;
    if (lo <= hi) return mid >= lo && mid <= hi;
    return mid >= lo || mid <= hi; // wraparound, e.g. 350°–10°
  }

  function toXY(bearingDeg, r) {
    const rad = ((bearingDeg - 90) * Math.PI) / 180;
    return [cx + r * Math.cos(rad), cy + r * Math.sin(rad)];
  }

  function sectorPath(i) {
    const a0 = i * SECTOR, a1 = a0 + SECTOR;
    const [ox0, oy0] = toXY(a0, outerR);
    const [ox1, oy1] = toXY(a1, outerR);
    const [ix0, iy0] = toXY(a0, innerR);
    const [ix1, iy1] = toXY(a1, innerR);
    return `M ${ox0} ${oy0} A ${outerR} ${outerR} 0 0 1 ${ox1} ${oy1} L ${ix1} ${iy1} A ${innerR} ${innerR} 0 0 0 ${ix0} ${iy0} Z`;
  }

  return (
    <svg width={size} height={size} style={{ display: 'block', margin: '4px auto 0' }}>
      {Array.from({ length: N }, (_, i) => (
        <path key={i} d={sectorPath(i)}
          fill={sectorActive(i) ? '#e040fb' : '#252535'}
          stroke="#111" strokeWidth="0.8" />
      ))}
      <text x={cx} y={10} textAnchor="middle" fill="#888" fontSize="7" fontFamily="sans-serif" fontWeight="bold">N</text>
      <circle cx={cx} cy={cy} r={2.5} fill="#555" />
    </svg>
  );
}

function parseWktLinestring(wkt) {
  const m = wkt?.match(/LINESTRING\s*\(([^)]+)\)/i);
  if (!m) return null;
  return m[1].trim().split(',').map(pair => {
    const [lon, lat] = pair.trim().split(/\s+/).map(Number);
    return [lon, lat];
  });
}

function destinationPoint(lon, lat, bearingDeg, distM) {
  const R = 6371000;
  const φ1 = lat * Math.PI / 180;
  const λ1 = lon * Math.PI / 180;
  const θ  = bearingDeg * Math.PI / 180;
  const δ  = distM / R;
  const φ2 = Math.asin(Math.sin(φ1)*Math.cos(δ) + Math.cos(φ1)*Math.sin(δ)*Math.cos(θ));
  const λ2 = λ1 + Math.atan2(Math.sin(θ)*Math.sin(δ)*Math.cos(φ1), Math.cos(δ) - Math.sin(φ1)*Math.sin(φ2));
  return [λ2 * 180 / Math.PI, φ2 * 180 / Math.PI];
}

function bearingConeGeoJSON(lon, lat, lbDeg, ubDeg, radiusM) {
  const center = [lon, lat];
  let start = lbDeg;
  let sweep = ((ubDeg - lbDeg) + 360) % 360;
  if (sweep < 2) { start -= (2 - sweep) / 2; sweep = 2; } // minimum visual width for TPEG
  const STEPS = 48;
  const ring = [center];
  for (let i = 0; i <= STEPS; i++) ring.push(destinationPoint(lon, lat, start + sweep * i / STEPS, radiusM));
  ring.push(center);
  return { type: 'FeatureCollection', features: [{ type: 'Feature', geometry: { type: 'Polygon', coordinates: [ring] }, properties: {} }] };
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function compassBearing(lon1, lat1, lon2, lat2) {
  const dLon = (lon2 - lon1) * Math.PI / 180;
  const φ1 = lat1 * Math.PI / 180, φ2 = lat2 * Math.PI / 180;
  const y = Math.sin(dLon) * Math.cos(φ2);
  const x = Math.cos(φ1) * Math.sin(φ2) - Math.sin(φ1) * Math.cos(φ2) * Math.cos(dLon);
  return (Math.atan2(y, x) * 180 / Math.PI + 360) % 360;
}

function fmtDist(m) {
  if (m < 1000) return `${Math.round(m)} m`;
  return `${(m / 1000).toFixed(2)} km`;
}

// Perpendicular distance from (px,py) to segment (ax,ay)-(bx,by), plain
// lon/lat degree space — only used to rank candidate waypoint pairs against
// each other, so no need for a proper great-circle metric.
function pointToSegmentDist(px, py, ax, ay, bx, by) {
  const dx = bx - ax, dy = by - ay;
  const len2 = dx * dx + dy * dy;
  if (len2 === 0) return Math.hypot(px - ax, py - ay);
  let t = ((px - ax) * dx + (py - ay) * dy) / len2;
  t = Math.max(0, Math.min(1, t));
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
}

// Cumulative distance (plain lon/lat degree units, not geodesic — only used
// to compare positions against each other) along `coords` to the point on it
// nearest (lon,lat). Lets us order a click against each waypoint's own
// position along the *actual routed geometry*, including the recoverable
// POFF/NOFF overshoot before the first / after the last waypoint (the
// straight-chord heuristic below has no representation for either, since it
// only ever considers gaps *between* existing waypoints).
function arcLengthAlongPolyline(coords, lon, lat) {
  if (!coords || coords.length < 2) return 0;
  let bestDist = Infinity, bestArc = 0, cum = 0;
  for (let i = 0; i < coords.length - 1; i++) {
    const [ax, ay] = coords[i], [bx, by] = coords[i + 1];
    const dx = bx - ax, dy = by - ay;
    const len = Math.hypot(dx, dy);
    const len2 = dx * dx + dy * dy;
    let t = len2 === 0 ? 0 : ((lon - ax) * dx + (lat - ay) * dy) / len2;
    t = Math.max(0, Math.min(1, t));
    const d = Math.hypot(lon - (ax + t * dx), lat - (ay + t * dy));
    if (d < bestDist) { bestDist = d; bestArc = cum + t * len; }
    cum += len;
  }
  return bestArc;
}

// Which gap a click point falls into — used to decide the insertion index
// for the drag-to-insert gesture. When `routeGeometry` (the actual routed
// road polyline, including any valid-node-expansion overshoot at either
// end) is available, orders the click against each waypoint's own arc-length
// position along it — so a click past the last waypoint (in the overshoot
// that the neg-offset stub recovers) correctly appends a new final waypoint
// instead of always landing between the first two (see project notes: a
// straight waypoint-to-waypoint chord heuristic has no index for "beyond the
// last waypoint" at all when there are only two waypoints, since its only
// candidate gap is the single (0,1) pair). Falls back to the straight-chord
// heuristic if no route geometry is loaded yet.
function nearestWaypointPairIndex(waypoints, lon, lat, routeGeometry) {
  if (routeGeometry?.length >= 2) {
    const clickArc = arcLengthAlongPolyline(routeGeometry, lon, lat);
    const arcs = waypoints.map(w => arcLengthAlongPolyline(routeGeometry, w.lon, w.lat));
    const idx = arcs.findIndex(a => a > clickArc);
    return idx === -1 ? waypoints.length : idx;
  }
  let bestIdx = 1, bestDist = Infinity;
  for (let i = 0; i < waypoints.length - 1; i++) {
    const a = waypoints[i], b = waypoints[i + 1];
    const d = pointToSegmentDist(lon, lat, a.lon, a.lat, b.lon, b.lat);
    if (d < bestDist) { bestDist = d; bestIdx = i + 1; }
  }
  return bestIdx;
}

// Interpolated midpoint of a polyline by vertex index — handles 2-vertex segments
// (where Math.floor(n/2) = 1 = endpoint) by interpolating between the two flanking vertices.
function polylineMid(coords) {
  if (!coords?.length) return null;
  if (coords.length === 1) return coords[0];
  const t = (coords.length - 1) / 2;
  const i = Math.floor(t), j = Math.ceil(t);
  if (i === j) return coords[i];
  const f = t - i;
  return [coords[i][0] + f * (coords[j][0] - coords[i][0]),
          coords[i][1] + f * (coords[j][1] - coords[i][1])];
}

function parseLatLon(str) {
  const m = str.trim().match(/^(-?\d+(?:\.\d+)?)[,\s]+(-?\d+(?:\.\d+)?)$/);
  if (!m) return null;
  const lat = parseFloat(m[1]), lon = parseFloat(m[2]);
  if (lat < -90 || lat > 90 || lon < -180 || lon > 180) return null;
  return { lat, lon };
}


// Clip a polyline [lon,lat][] to start at the nearest point to (snapLon, snapLat).
// Returns the tail portion of the polyline from that snap point onward.
function clipGeomFromPoint(coords, snapLon, snapLat) {
  if (!coords || coords.length < 2) return coords;
  let bestIdx = 0, bestT = 0, bestDist = Infinity;
  for (let i = 0; i < coords.length - 1; i++) {
    const ax = coords[i][0], ay = coords[i][1];
    const bx = coords[i + 1][0], by = coords[i + 1][1];
    const dx = bx - ax, dy = by - ay;
    const len2 = dx * dx + dy * dy;
    const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((snapLon - ax) * dx + (snapLat - ay) * dy) / len2));
    const ex = snapLon - (ax + t * dx), ey = snapLat - (ay + t * dy);
    const d = ex * ex + ey * ey;
    if (d < bestDist) { bestDist = d; bestIdx = i; bestT = t; }
  }
  const clipPt = [
    coords[bestIdx][0] + bestT * (coords[bestIdx + 1][0] - coords[bestIdx][0]),
    coords[bestIdx][1] + bestT * (coords[bestIdx + 1][1] - coords[bestIdx][1]),
  ];
  return [clipPt, ...coords.slice(bestIdx + 1)];
}

// Clip a polyline [lon,lat][] to end at the nearest point to (snapLon, snapLat).
// Returns the head portion of the polyline up to that snap point.
function clipGeomToPoint(coords, snapLon, snapLat) {
  if (!coords || coords.length < 2) return coords;
  let bestIdx = 0, bestT = 0, bestDist = Infinity;
  for (let i = 0; i < coords.length - 1; i++) {
    const ax = coords[i][0], ay = coords[i][1];
    const bx = coords[i + 1][0], by = coords[i + 1][1];
    const dx = bx - ax, dy = by - ay;
    const len2 = dx * dx + dy * dy;
    const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((snapLon - ax) * dx + (snapLat - ay) * dy) / len2));
    const ex = snapLon - (ax + t * dx), ey = snapLat - (ay + t * dy);
    const d = ex * ex + ey * ey;
    if (d < bestDist) { bestDist = d; bestIdx = i; bestT = t; }
  }
  const clipPt = [
    coords[bestIdx][0] + bestT * (coords[bestIdx + 1][0] - coords[bestIdx][0]),
    coords[bestIdx][1] + bestT * (coords[bestIdx + 1][1] - coords[bestIdx][1]),
  ];
  return [...coords.slice(0, bestIdx + 1), clipPt];
}

// ── Custom marker images ──────────────────────────────────────────────────────

function addMapImages(map) {
  // Displacement arrowhead — points north (↑) by default, tip at top-center.
  // Placed at the snap coordinate with icon-anchor:'top' and rotated by LRP→snap
  // bearing, so the tip always lands on the snap point and shaft trails toward LRP.
  const aw = 12, ah = 16;
  const arrowCanvas = document.createElement('canvas');
  arrowCanvas.width = aw; arrowCanvas.height = ah;
  const ac = arrowCanvas.getContext('2d');
  ac.clearRect(0, 0, aw, ah);
  ac.beginPath();
  ac.moveTo(aw / 2, 1);       // tip — top-center
  ac.lineTo(1,      ah - 1);  // bottom-left
  ac.lineTo(aw - 1, ah - 1);  // bottom-right
  ac.closePath();
  ac.fillStyle   = 'rgba(255,255,255,0.9)';
  ac.strokeStyle = 'rgba(0,0,0,0.6)';
  ac.lineWidth   = 1.5;
  ac.fill(); ac.stroke();
  map.addImage('displacement-arrow', ac.getImageData(0, 0, aw, ah));

  // Direction triangle — solid filled, points right (→), rotated by MapLibre to follow
  // the line direction. Registered as SDF so icon-color can tint it per layer.
  const tw = 14, th = 14;
  const triCanvas = document.createElement('canvas');
  triCanvas.width = tw; triCanvas.height = th;
  const tc = triCanvas.getContext('2d');
  tc.clearRect(0, 0, tw, th);
  // Solid white fill (SDF tinting overrides colour at render time)
  tc.beginPath();
  tc.moveTo(2, 1); tc.lineTo(tw - 1, th / 2); tc.lineTo(2, th - 1);
  tc.closePath();
  tc.fillStyle = 'white';
  tc.fill();
  map.addImage('direction-triangle', tc.getImageData(0, 0, tw, th), { sdf: true });
  // Keep legacy name so any surviving refs still resolve
  map.addImage('candidate-chevron',  tc.getImageData(0, 0, tw, th), { sdf: true });

  // Numbered LRP marker circles (1–20). Canvas-drawn so no glyph/font dependency.
  const ms = 24;
  for (let n = 1; n <= 20; n++) {
    const mc = document.createElement('canvas');
    mc.width = ms; mc.height = ms;
    const mc2d = mc.getContext('2d');
    mc2d.beginPath();
    mc2d.arc(ms / 2, ms / 2, ms / 2 - 2, 0, 2 * Math.PI);
    mc2d.fillStyle = '#e040fb';
    mc2d.fill();
    mc2d.strokeStyle = '#ffffff';
    mc2d.lineWidth = 2;
    mc2d.stroke();
    mc2d.fillStyle = '#ffffff';
    mc2d.font = `bold ${n > 9 ? 9 : 11}px Arial, sans-serif`;
    mc2d.textAlign = 'center';
    mc2d.textBaseline = 'middle';
    mc2d.fillText(String(n), ms / 2, ms / 2 + 0.5);
    map.addImage(`lrp-num-${n}`, mc2d.getImageData(0, 0, ms, ms));
  }

}

// ── Map Component ──────────────────────────────────────────────────────────────

export default function MapView({ tilesBase, ready, initialBounds }) {
  const mapContainer    = useRef(null);
  const mapRef          = useRef(null);
  const tileCacheRef    = useRef(new Map());
  const nodesCacheRef   = useRef(new Map());
  const pendingCountRef = useRef(0);
  const pmtilesRef      = useRef(null);
  // Captured once, before the map is constructed, so a later effect can tell
  // whether the page loaded with an explicit #hash (bookmark/share link) —
  // if so, the auto-fit-to-dataset-bounds effect below must not override it.
  const initialHashRef  = useRef(null);
  // Set on the first user-originated pan/zoom (`e.originalEvent` is only
  // present for real input, not for our own `fitBounds`/`jumpTo` calls) —
  // guards against yanking the view out from under someone who started
  // exploring before the dataset bounds fetch resolved.
  const userInteractedRef = useRef(false);
  const pulseRef        = useRef(null);
  const frontierPulseRef = useRef(null);
  const lrpPanelRef     = useRef(null);
  const segPanelRef     = useRef(null);
  // Incremental replay state — avoids O(N²) recomputation when stepping forward
  const replayVisualRef = useRef(null);   // last computed visual state
  const replayStepRef   = useRef(-1);     // step index of replayVisualRef
  const replayStepsKey  = useRef(null);   // identity check for replaySteps array
  const flashAnimRef    = useRef(null);   // rAF handle for sonar-ping fade animation
  const routePulseRef   = useRef(null);   // rAF handle for route-found pulse animation
  const decodePulseRef  = useRef(null);   // rAF handle for post-decode green→cyan fade animation
  const palPulseRef     = useRef(null);   // rAF handle for PAL point pulsing ring
  const candPanelRef        = useRef(null);
  const candidatePopupRef   = useRef(null);
  const capturePopupRef     = useRef(null);
  const pendingPopupCoordRef    = useRef(null); // geographic coord to project after fitBounds animation
  const pendingCandAnchorCoordRef = useRef(null); // candidate popup snap coord, same deferred scheme

  const [status, setStatus] = useState(null);
  const [infoProps, setInfoProps] = useState(null);
  const [infoAnchor, setInfoAnchor] = useState(null);
  const [lrpInfo, setLrpInfo] = useState(null);
  const [nodeInfo, setNodeInfo] = useState(null);
  const [nodeAnchor, setNodeAnchor] = useState(null);
  const [candAnchor, setCandAnchor] = useState(null);
  const [basemap, setBasemap] = useState('liberty');
  const [segDiagnosis, setSegDiagnosis] = useState(null);

  const [measuring, setMeasuring] = useState(false);
  const [measurePts, setMeasurePts] = useState([]);
  const [measureCursor, setMeasureCursor] = useState(null);
  const measuringRef  = useRef(false);
  const measurePtsRef = useRef([]);

  const [coordCaptureActive, setCoordCaptureActive] = useState(false);
  const coordCaptureActiveRef = useRef(false);
  const [cursorCoord, setCursorCoord] = useState(null);
  const cursorCoordRef = useRef(null);
  const [coordCopied, setCoordCopied] = useState(false);
  const [copiedText, setCopiedText] = useState('');
  const [locPins, setLocPins] = useState([]);
  const locPinMarkersRef = useRef({});
  const [showZoomPanel, setShowZoomPanel] = useState(false);
  const [zoomInput, setZoomInput] = useState('');
  const [zoomError, setZoomError] = useState(false);
  const [bearingActive, setBearingActive] = useState(false);
  const bearingActiveRef = useRef(false);
  const [bearingPts, setBearingPts] = useState([]);
  const bearingPtsRef = useRef([]);
  const [permalinkCopied, setPermalinkCopied] = useState(false);
  const [toolbarOpen, setToolbarOpen] = useState(false);

  const { pos: lrpPos,  onMouseDown: lrpMouseDown,  resetPos: lrpResetPos  } = useDraggable(lrpPanelRef);
  const { pos: segPos,  onMouseDown: segMouseDown,  resetPos: segResetPos  } = useDraggable(segPanelRef);
  const { pos: candPos, onMouseDown: candMouseDown, resetPos: candResetPos } = useDraggable(candPanelRef);

  const decodeResult               = useStore(s => s.decodeResult);
  const highlightedSegment         = useStore(s => s.highlightedSegment);
  const setHighlightedSegment      = useStore(s => s.setHighlightedSegment);
  const requestedInfoSegment       = useStore(s => s.requestedInfoSegment);
  const clearRequestedInfoSegment  = useStore(s => s.clearRequestedInfoSegment);
  const traceHighlightSegIds  = useStore(s => s.traceHighlightSegIds);
  const traceHighlightSnaps   = useStore(s => s.traceHighlightSnaps);
  const traceLrpFocus         = useStore(s => s.traceLrpFocus);
  const setTraceLrpFocus      = useStore(s => s.setTraceLrpFocus);
  const mapFlyTo              = useStore(s => s.mapFlyTo);
  const showSegmentLayer      = useStore(s => s.showSegmentLayer);
  const searchRadiusM         = useStore(s => s.params.candidate_search_radius_m);
  const lfrcnpTolerance       = useStore(s => s.params.lfrcnp_tolerance ?? 0);
  // In encode mode these read the round-trip verify-decode's replay data
  // instead of the last manual decode's.
  const replayStep  = useStore(s => s.mode === 'encode' ? s.verifyReplayStep  : s.replayStep);
  const replaySteps = useStore(s => s.mode === 'encode' ? s.verifyReplaySteps : s.replaySteps);
  const replayStats = useStore(s => s.mode === 'encode' ? s.verifyReplayStats : s.replayStats);
  const showReplay         = useStore(s => s.showReplay);
  const showTrace          = useStore(s => s.showTrace);
  const candidatePopup     = useStore(s => s.candidatePopup);
  const clearCandidatePopup = useStore(s => s.clearCandidatePopup);

  // ── Encode mode ───────────────────────────────────────────────────────────
  const mode          = useStore(s => s.mode);
  const locationType  = useStore(s => s.locationType);
  const waypoints     = useStore(s => s.waypoints);
  const liveRoute      = useStore(s => s.liveRoute);
  const addWaypoint    = useStore(s => s.addWaypoint);
  const moveWaypoint   = useStore(s => s.moveWaypoint);
  const encodeModeRef  = useRef(false);
  const waypointMarkersRef = useRef([]);
  const snapPickerPopupRef = useRef(null);
  const suppressNextClickRef = useRef(false);
  // Assigned once, inside the map-load effect, to the (kind, index, e) => void
  // drag-starter — called from the separate waypoint-marker effect's own
  // per-marker mousedown listener (marker "move" drags start on the marker's
  // own DOM element, not the map canvas, so they can't share a mousedown
  // handler directly; this ref is how the two effects meet in the middle).
  const startEncodeDragRef = useRef(null);
  const insertWaypoint = useStore(s => s.insertWaypoint);
  const removeWaypoint = useStore(s => s.removeWaypoint);
  const waypointHistory = useStore(s => s.waypointHistory);
  const clearWaypoints  = useStore(s => s.clearWaypoints);
  const undoWaypoint     = useStore(s => s.undo);
  const openResult        = useStore(s => s.openResult);
  const showResult        = useStore(s => s.showResult);

  // Only one map popup (segment / LRP / node / candidate) may be visible at a time —
  // call this before opening any of them so the others don't linger on screen.
  const closeAllPopups = () => {
    setInfoProps(null);
    setInfoAnchor(null);
    setLrpInfo(null);
    setSegDiagnosis(null);
    setNodeInfo(null);
    setNodeAnchor(null);
    clearCandidatePopup();
    setCandAnchor(null);
  };

  const openlrString    = useStore(s => s.openlrString);
  const setOpenlrString = useStore(s => s.setOpenlrString);
  const runDecode       = useStore(s => s.runDecode);

  const permalinkAutoLoaded = useRef(false);
  useEffect(() => {
    if (!ready || permalinkAutoLoaded.current) return;
    const hash = window.location.hash;
    if (hash.startsWith('#q=')) {
      const q = decodeURIComponent(hash.slice(3));
      if (q) {
        permalinkAutoLoaded.current = true;
        setOpenlrString(q);
        runDecode();
      }
    }
  }, [ready]); // eslint-disable-line react-hooks/exhaustive-deps

  // Reset drag position when a new popup target is clicked
  useEffect(() => { lrpResetPos(); }, [lrpInfo]);   // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { segResetPos(); }, [infoProps]);  // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {                                  // eslint-disable-line react-hooks/exhaustive-deps
    candidatePopupRef.current = candidatePopup;
    candResetPos();
    if (candidatePopup) {
      // Opening a candidate popup (e.g. from TracePanel) should close any other
      // popup — but not itself, so don't route this through closeAllPopups().
      setInfoProps(null);
      setInfoAnchor(null);
      setLrpInfo(null);
      setSegDiagnosis(null);
      setNodeInfo(null);
      setNodeAnchor(null);
    }
    const map = mapRef.current;

    // Update trace-segment-line color and trace-segment-arrow visibility based on accept/reject
    const isAccepted = candidatePopup?.winner || candidatePopup?.ctype === 'accepted';
    if (map?.getLayer('trace-segment-line')) {
      map.setPaintProperty('trace-segment-line', 'line-color',
        candidatePopup ? (isAccepted ? '#22cc66' : '#ee4444') : '#ff8c00');
      map.setPaintProperty('trace-segment-line', 'line-width', candidatePopup ? 5 : 4);
    }
    if (map?.getLayer('trace-segment-arrow')) {
      map.setLayoutProperty('trace-segment-arrow', 'visibility',
        candidatePopup ? 'visible' : 'none');
      if (candidatePopup) {
        const arrowColor = isAccepted ? '#22cc66' : '#ee4444';
        map.setPaintProperty('trace-segment-arrow', 'icon-color',       arrowColor);
        map.setPaintProperty('trace-segment-arrow', 'icon-halo-color',  'white');
        map.setPaintProperty('trace-segment-arrow', 'icon-halo-width',  4);
        map.setPaintProperty('trace-segment-arrow', 'icon-halo-blur',   0);
        map.setPaintProperty('trace-segment-arrow', 'icon-opacity',     1);
      }
    }

    if (!candidatePopup?.snap_lon) {
      setCandAnchor(null);
      pendingCandAnchorCoordRef.current = null;
      if (map?.getLayer('replay-candidates-arrow')) {
        map.setLayoutProperty('replay-candidates-arrow', 'visibility', 'none');
        map.setFilter('replay-candidates-arrow', null);
      }
      if (map?.getLayer('replay-candidates-line'))
        map.setFilter('replay-candidates-line', null);
      // Restore snap markers and displacement lines to winning-candidate positions.
      const lrps = decodeResultRef.current?.lrps ?? [];
      if (map?.getSource('lrp-snap')) {
        map.getSource('lrp-snap').setData({
          type: 'FeatureCollection',
          features: lrps.filter(l => l.snap_lon != null).map((lrp, idx) => ({
            type: 'Feature',
            geometry: { type: 'Point', coordinates: [lrp.snap_lon, lrp.snap_lat] },
            properties: {
              index: idx,
              is_endpoint: lrp.snap_is_endpoint ?? false,
              bearing: compassBearing(lrp.lon, lrp.lat, lrp.snap_lon, lrp.snap_lat),
            },
          })),
        });
        map.getSource('lrp-displacement').setData({
          type: 'FeatureCollection',
          features: lrps.filter(l => l.snap_lon != null).map((lrp, idx) => ({
            type: 'Feature',
            geometry: { type: 'LineString', coordinates: [[lrp.lon, lrp.lat], [lrp.snap_lon, lrp.snap_lat]] },
            properties: { index: idx },
          })),
        });
      }
      return;
    }
    if (!map) return;

    // Show only the clicked LRP's tether, pointing to this candidate's snap point.
    // All other LRPs' displacement lines are hidden while the popup is open so the
    // user sees only the snap relevant to the candidate they selected.
    const popupLrpIdx = candidatePopup.lrp_idx;
    const lrps = decodeResultRef.current?.lrps ?? [];
    if (map.getSource('lrp-snap') && popupLrpIdx != null) {
      const lrp = lrps[popupLrpIdx];
      if (lrp) {
        map.getSource('lrp-snap').setData({
          type: 'FeatureCollection',
          features: [{
            type: 'Feature',
            geometry: { type: 'Point', coordinates: [candidatePopup.snap_lon, candidatePopup.snap_lat] },
            properties: {
              index: popupLrpIdx,
              is_endpoint: false,
              bearing: compassBearing(lrp.lon, lrp.lat, candidatePopup.snap_lon, candidatePopup.snap_lat),
            },
          }],
        });
        map.getSource('lrp-displacement').setData({
          type: 'FeatureCollection',
          features: [{
            type: 'Feature',
            geometry: { type: 'LineString', coordinates: [[lrp.lon, lrp.lat], [candidatePopup.snap_lon, candidatePopup.snap_lat]] },
            properties: { index: popupLrpIdx },
          }],
        });
      }
    }

    // Defer anchor projection until after the fitBounds animation (triggered by
    // the traceHighlightSegIds effect in the same render cycle) completes.
    // setTimeout(0) fallback handles the case where the segment was already
    // highlighted (no fitBounds called), so traceHighlightSegIds effect won't fire.
    // Anchor to segment midpoint (not snap point, which lands at or near an endpoint)
    const segFeat = candidatePopup.segment_id != null ? getSegGeomCache().get(candidatePopup.segment_id) : null;
    const anchorCoord = polylineMid(segFeat?.geometry?.coordinates)
      ?? [candidatePopup.snap_lon, candidatePopup.snap_lat];
    pendingCandAnchorCoordRef.current = anchorCoord;
    setCandAnchor(null);
    const fallbackId = setTimeout(() => {
      if (pendingCandAnchorCoordRef.current) {
        pendingCandAnchorCoordRef.current = null;
        const pt = mapRef.current?.project(anchorCoord);
        if (pt) setCandAnchor({ x: pt.x, y: pt.y });
      }
    }, 0);

    // Show direction arrows for the selected candidate only
    if (map.getLayer('replay-candidates-arrow') && candidatePopup.segment_id != null) {
      map.setFilter('replay-candidates-arrow', ['all',
        ['==', ['get', 'segment_id'], candidatePopup.segment_id],
        ['==', ['get', 'traversal'],  candidatePopup.traversal ?? ''],
      ]);
      map.setLayoutProperty('replay-candidates-arrow', 'visibility', 'visible');
    }
    // Hide the replay candidate lines for this segment so the trace-highlight
    // segment rendering isn't obscured by the replay overlay.
    if (map.getLayer('replay-candidates-line') && candidatePopup.segment_id != null) {
      map.setFilter('replay-candidates-line', ['!=', ['get', 'segment_id'], candidatePopup.segment_id]);
    }
    return () => clearTimeout(fallbackId);
  }, [candidatePopup]);

  // Open the segment info popup when ResultPanel (or decoded-path click) requests it.
  useEffect(() => {
    if (!requestedInfoSegment) return;
    const { tile, local_index } = requestedInfoSegment;
    clearRequestedInfoSegment();
    const [z, x, y] = tile.split('/').map(Number);
    const segId = getSegmentId(z, x, y, local_index);
    const feat = getSegGeomCache().get(segId);
    if (!feat) return;

    // Set popup content immediately, but defer anchor projection.
    // The highlightedSegment effect (running in the same render cycle) will call
    // fitBounds and then register the moveend listener — storing the target coord
    // in pendingPopupCoordRef lets it project AFTER the animation settles.
    closeAllPopups();
    setInfoProps({ ...feat.properties, segment_id: segId });

    const coords = feat.geometry.coordinates;
    pendingPopupCoordRef.current = polylineMid(coords);
  }, [requestedInfoSegment]); // eslint-disable-line react-hooks/exhaustive-deps

  // Always-current ref so the highlight effect can read decodeResult without
  // adding it to the dependency array (which would cause both effects to race).
  const decodeResultRef = useRef(decodeResult);
  useEffect(() => { decodeResultRef.current = decodeResult; }, [decodeResult]);

  // Store tilesBase in a ref so the loadVisibleTiles callback can see the latest value
  const tilesBaseRef = useRef(tilesBase);
  useEffect(() => { tilesBaseRef.current = tilesBase; }, [tilesBase]);

  // ── Init map ────────────────────────────────────────────────────────────────

  useEffect(() => {
    if (mapRef.current) return; // already initialized

    // Captured before construction — the map's own `hash: true` option starts
    // rewriting the URL on the very first `moveend` (including our synthetic
    // initial view), so reading `window.location.hash` any later would no
    // longer reflect what the page actually loaded with.
    initialHashRef.current = window.location.hash;

    const map = new maplibregl.Map({
      container: mapContainer.current,
      style:     'https://tiles.openfreemap.org/styles/liberty',
      // Conservative default: the whole world. Overridden by the effect below
      // once the configured PMTiles archive's own bounds are known (or left
      // as-is if that lookup fails or the page loaded with an explicit
      // #hash — see the effect's comment).
      center:    [0, 0],
      zoom:      1,
      hash:      true,
      // This is a flat, north-up routing tool — rotate/pitch is never used
      // anywhere in the app. Disabled outright so the right mouse button
      // (which MapLibre's default drag-rotate handler otherwise claims) is
      // free for encode mode's right-click waypoint editing.
      dragRotate: false,
      pitchWithRotate: false,
      touchPitch: false,
    });
    mapRef.current = map;

    const markInteracted = e => { if (e.originalEvent) userInteractedRef.current = true; };
    map.on('dragstart', markInteracted);
    map.on('zoomstart', markInteracted);

    // The right mouse button is repurposed for encode-mode waypoint editing
    // (see the mousedown handlers below) — suppress the native browser
    // context menu everywhere on the map so it doesn't appear alongside.
    map.getCanvasContainer().addEventListener('contextmenu', e => e.preventDefault());

    map.addControl(new maplibregl.NavigationControl(), 'top-right');

    // Re-add custom images whenever the style reloads (initial load + basemap switches).
    map.on('style.load', () => addMapImages(map));

    map.on('load', () => {
      // ── OLR segment source ────────────────────────────────────────────────
      map.addSource('olr-segments', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      for (let frc = 0; frc < 8; frc++) {
        map.addLayer({
          id:     `olr-frc${frc}`,
          type:   'line',
          source: 'olr-segments',
          filter: ['==', ['get', 'frc'], frc],
          layout: { visibility: 'none' }, // hidden until user enables Segments layer
          paint: {
            'line-color': FRC_COLOR[frc],
            'line-width': ['interpolate', ['linear'], ['zoom'], 10, FRC_WIDTH[frc] * 2.0, 16, FRC_WIDTH[frc] * 5.5],
            'line-opacity': 0.9,
          },
        });
      }

      // Highlight layer — activated on click or result-panel selection
      map.addLayer({
        id:     'olr-highlight',
        type:   'line',
        source: 'olr-segments',
        filter: ['boolean', false],
        layout: { visibility: 'none' }, // follows segment layer visibility
        paint: {
          'line-color':   '#ffe000',
          'line-width':   6,
          'line-opacity': 1,
        },
      });

      // ── Node intersection markers ─────────────────────────────────────────
      map.addSource('olr-nodes', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });
      map.addLayer({
        id:     'olr-nodes-circle',
        type:   'circle',
        source: 'olr-nodes',
        layout: { visibility: 'none' },
        paint: {
          'circle-radius':       5,
          'circle-color':        '#ffffff',
          'circle-stroke-width': 1.5,
          'circle-stroke-color': '#444444',
          'circle-opacity':      0.9,
        },
      });

      // ── Decoded path source + layer ───────────────────────────────────────
      map.addSource('decoded-path', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      map.addLayer({
        id:     'decoded-path-line',
        type:   'line',
        source: 'decoded-path',
        paint: {
          'line-color':   '#00d4ff',
          'line-width':   5,
          'line-opacity': 0.9,
        },
      });
      // Direction on the decoded path is conveyed by the numbered LRP markers (1 → N).

      // ── Offset uncertainty caps (v3 [LB, UB] zone at path head/tail) ────
      // Path is now trimmed at LB, so these caps sit at the very START/END of
      // the solid path — no overlap.  Darker cyan dashes read as "same thing,
      // but uncertain" without any z-order tricks.
      map.addSource('offset-uncertainty', { type: 'geojson', data: { type: 'FeatureCollection', features: [] } });
      map.addLayer({
        id: 'offset-uncertainty-line', type: 'line', source: 'offset-uncertainty',
        paint: {
          'line-color':     '#0088bb',
          'line-width':     5,
          'line-opacity':   0.95,
          'line-dasharray': [1, 0.5],
        },
      });

      // ── LRP marker source + layer ─────────────────────────────────────────
      map.addSource('lrp-markers', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      // Single icon layer: canvas-generated numbered circles (no glyph/font dependency).
      // ID kept as 'lrp-markers-circle' so existing beforeId refs in layers below still work.
      map.addLayer({
        id:     'lrp-markers-circle',
        type:   'symbol',
        source: 'lrp-markers',
        layout: {
          'icon-image':             ['concat', 'lrp-num-', ['to-string', ['+', ['get', 'index'], 1]]],
          'icon-allow-overlap':     true,
          'icon-ignore-placement':  true,
        },
      });

      // ── LRP snap displacement lines (encoded coord → snap point) ─────────
      map.addSource('lrp-displacement', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });
      map.addLayer({
        id: 'lrp-displacement-line', type: 'line', source: 'lrp-displacement',
        paint: {
          'line-color':     '#000000',
          'line-width':     1.5,
          'line-opacity':   0.7,
          'line-dasharray': [3, 4],
        },
      }, 'lrp-markers-circle');

      // ── LRP snap arrowhead source (arrow tip at snap coord, rotated to LRP→snap bearing) ──
      map.addSource('lrp-snap', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      // ── Arrowhead at snap point (tip on road, shaft trailing toward LRP) ────
      map.addLayer({
        id: 'lrp-displacement-arrow', type: 'symbol', source: 'lrp-snap',
        layout: {
          'icon-image':             'displacement-arrow',
          'icon-rotate':            ['get', 'bearing'],
          'icon-rotation-alignment': 'map',
          'icon-anchor':            'top',   // tip of arrow at snap coord; shaft trails back
          'icon-size':              1.0,
          'icon-allow-overlap':     true,
          'icon-ignore-placement':  true,
        },
      }, 'lrp-markers-circle');


      // ── LRP bearing cone (shown when an LRP is selected) ─────────────────
      map.addSource('lrp-bearing', { type: 'geojson', data: { type: 'FeatureCollection', features: [] } });
      map.addLayer({
        id: 'lrp-bearing-fill', type: 'fill', source: 'lrp-bearing',
        paint: { 'fill-color': '#aa00ff', 'fill-opacity': 0.18 },
      }, 'lrp-markers-circle');
      map.addLayer({
        id: 'lrp-bearing-outline', type: 'line', source: 'lrp-bearing',
        paint: { 'line-color': '#aa00ff', 'line-width': 1.5, 'line-opacity': 0.8 },
      }, 'lrp-markers-circle');

      // ── Highlighted segment (sits above everything else) ──────────────────
      map.addSource('highlighted-segment', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      map.addLayer({
        id:     'highlighted-segment-halo',
        type:   'line',
        source: 'highlighted-segment',
        paint: {
          'line-color':   '#ffffff',
          'line-width':   14,
          'line-opacity': 0.6,
        },
      });

      map.addLayer({
        id:     'highlighted-segment-line',
        type:   'line',
        source: 'highlighted-segment',
        paint: {
          'line-color':   '#ffe000',
          'line-width':   6,
          'line-opacity': 1,
        },
      });

      // ── Trace-panel highlight (separate from result-panel highlight) ───────
      map.addSource('trace-segment', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });
      map.addLayer({
        id:     'trace-segment-halo',
        type:   'line',
        source: 'trace-segment',
        paint: {
          'line-color':   '#ff8c00',
          'line-width':   14,
          'line-opacity': 0.4,
          'line-blur':    3,
        },
      });
      map.addLayer({
        id:     'trace-segment-line',
        type:   'line',
        source: 'trace-segment',
        paint: {
          'line-color':   '#ff8c00',
          'line-width':   4,
          'line-opacity': 1,
        },
      });
      // Direction triangles on the selected candidate segment (shown when candidatePopup is open)
      map.addLayer({
        id:     'trace-segment-arrow',
        type:   'symbol',
        source: 'trace-segment',
        layout: {
          'symbol-placement':    'line',
          'symbol-spacing':      18,
          'icon-image':          'direction-triangle',
          'icon-size':           1.4,
          'icon-allow-overlap':  true,
          'icon-ignore-placement': true,
          'visibility':          'none',
        },
        paint: {
          'icon-color':        'white',
          'icon-halo-color':   'white',
          'icon-halo-width':   4,
          'icon-halo-blur':    0,
          'icon-opacity':      1.0,
        },
      });

      // ── Replay sources & layers ──────────────────────────────────────────
      const emptyFC = { type: 'FeatureCollection', features: [] };

      map.addSource('replay-radius',     { type: 'geojson', data: emptyFC });
      map.addSource('replay-traversed',         { type: 'geojson', data: emptyFC });
      map.addSource('replay-route',             { type: 'geojson', data: emptyFC });
      map.addSource('replay-candidates',        { type: 'geojson', data: emptyFC });
      map.addSource('replay-cloud',      { type: 'geojson', data: emptyFC });
      map.addSource('replay-frontier',   { type: 'geojson', data: emptyFC });
      map.addSource('replay-leg',        { type: 'geojson', data: emptyFC });

      // Search radius ring — pulsing fill + dashed border
      map.addLayer({
        id: 'replay-radius-fill', type: 'fill', source: 'replay-radius',
        paint: { 'fill-color': '#aa44ff', 'fill-opacity': 0.06 },
      });
      map.addLayer({
        id: 'replay-radius-line', type: 'line', source: 'replay-radius',
        paint: { 'line-color': '#cc66ff', 'line-width': 2, 'line-opacity': 0.85, 'line-dasharray': [4, 3] },
      });

      // A* traversed edges and node cloud — drawn before the route so the route always overlays them
      map.addLayer({
        id: 'replay-traversed-line', type: 'line', source: 'replay-traversed',
        layout: { 'line-cap': 'butt', 'line-join': 'round' },
        paint: {
          'line-color':   '#cc4433',
          'line-width':   5,
          'line-opacity': 0.65,
        },
      });
      map.addLayer({
        id: 'replay-cloud-circle', type: 'circle', source: 'replay-cloud',
        paint: {
          'circle-radius':       3,
          'circle-opacity':      0.9,
          'circle-color':        '#6b0000',
          'circle-stroke-width': 0,
        },
      });

      // Found route — dark casing underneath keeps it readable over any basemap colour.
      map.addLayer({
        id: 'replay-route-casing', type: 'line', source: 'replay-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#0f172a', 'line-width': 10, 'line-opacity': 1.0 },
      });
      map.addLayer({
        id: 'replay-route-line', type: 'line', source: 'replay-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#16a34a', 'line-width': 6, 'line-opacity': 1.0 },
      });

      // Candidate segments — thick green (accepted) / thick red (rejected).
      // Bidirectional segments (has_counterpart=true): each direction is 4 px wide and
      // offset +2 px so the two lines touch back-to-back along the road centre.
      // The Backward candidate's reversed coordinates make its +2 offset land on the
      // physically opposite side, so they separate automatically.
      // Single-direction segments (has_counterpart=false): one 8 px line centred on the road.
      map.addLayer({
        id: 'replay-candidates-line', type: 'line', source: 'replay-candidates',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: {
          'line-color': ['case',
            ['boolean', ['get', 'winner'], false], '#00ff88',
            ['==', ['get', 'ctype'], 'accepted'],  '#22cc66',
            '#dd2222',
          ],
          'line-width': ['case',
            ['boolean', ['get', 'has_counterpart'], false], 4,
            6,
          ],
          'line-opacity': ['case',
            ['boolean', ['get', 'winner'], false], 1.0,
            ['==', ['get', 'ctype'], 'accepted'],  0.9,
            0.75,
          ],
          'line-offset': ['case',
            ['boolean', ['get', 'has_counterpart'], false], 2,
            0,
          ],
        },
      });
      // Direction triangles — hidden by default; shown only when a candidate is selected
      map.addLayer({
        id: 'replay-candidates-arrow', type: 'symbol', source: 'replay-candidates',
        layout: {
          'symbol-placement':      'line',
          'symbol-spacing':        18,
          'icon-image':            'direction-triangle',
          'icon-size':             1.0,
          'icon-allow-overlap':    true,
          'icon-ignore-placement': true,
          'visibility':            'none',
        },
        paint: {
          'icon-color':   'white',
          'icon-opacity': 0.9,
        },
      });

      // Frontier — bright white pulsing nodes
      map.addLayer({
        id: 'replay-frontier-circle', type: 'circle', source: 'replay-frontier',
        paint: {
          'circle-radius':       6,
          'circle-color':        '#ffffff',
          'circle-opacity':      0.95,
          'circle-blur':         0.3,
          'circle-stroke-width': 1.5,
          'circle-stroke-color': '#88ccff',
        },
      });

      // Leg from/to markers — inserted below lrp-markers-circle so LRP numbers stay readable
      map.addLayer({
        id: 'replay-leg-from', type: 'circle', source: 'replay-leg',
        filter: ['==', ['get', 'role'], 'from'],
        paint: { 'circle-radius': 9, 'circle-color': '#00ff88', 'circle-stroke-width': 2, 'circle-stroke-color': '#fff' },
      }, 'lrp-markers-circle');
      map.addLayer({
        id: 'replay-leg-to', type: 'circle', source: 'replay-leg',
        filter: ['==', ['get', 'role'], 'to'],
        paint: { 'circle-radius': 9, 'circle-color': '#ff4444', 'circle-stroke-width': 2, 'circle-stroke-color': '#fff' },
      }, 'lrp-markers-circle');

      // Sonar-ping ring — tracks the latest A* node; animated via RAF in the replay effect.
      map.addSource('replay-flash', { type: 'geojson', data: emptyFC });
      map.addLayer({
        id: 'replay-flash-ring', type: 'circle', source: 'replay-flash',
        paint: {
          'circle-radius':         20,
          'circle-color':          'transparent',
          'circle-stroke-width':   2.5,
          'circle-stroke-color':   '#00eeff',
          'circle-stroke-opacity': 1.0,
          'circle-opacity':        0,
        },
      });

      // ── Measurement tool sources + layers ────────────────────────────────
      const emptyFC2 = { type: 'FeatureCollection', features: [] };
      map.addSource('measure-line',   { type: 'geojson', data: emptyFC2 });
      map.addSource('measure-points', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'measure-line-layer', type: 'line', source: 'measure-line',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: {
          'line-color':     '#ffffff',
          'line-width':     2,
          'line-dasharray': [4, 4],
          'line-opacity':   0.9,
        },
      });
      map.addLayer({
        id: 'measure-points-layer', type: 'circle', source: 'measure-points',
        paint: {
          'circle-radius':       5,
          'circle-color':        '#ffffff',
          'circle-stroke-color': '#333333',
          'circle-stroke-width': 1.5,
        },
      });

      // ── PointAlongLine result marker ──────────────────────────────────────
      map.addSource('pal-point', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'pal-point-layer', type: 'circle', source: 'pal-point',
        paint: {
          'circle-radius':       7,
          'circle-color':        '#ff6b35',
          'circle-stroke-color': '#ffffff',
          'circle-stroke-width': 2.5,
          'circle-opacity':      0.9,
        },
      });
      // Pulsing ring — driven by rAF in the decode-result effect.
      map.addSource('pal-pulse', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'pal-pulse-ring', type: 'circle', source: 'pal-pulse',
        paint: {
          'circle-radius':         10,
          'circle-color':          'transparent',
          'circle-stroke-width':   2.5,
          'circle-stroke-color':   '#ff6b35',
          'circle-stroke-opacity': 0,
          'circle-opacity':        0,
        },
      });
      // POFF uncertainty: the v3-encoded offset is itself a quantization
      // bucket [lb, ub] — the true point could genuinely be anywhere in it.
      // Drawn the same dark-navy dashed style as Line decode's offset-
      // uncertainty caps (`offset-uncertainty-line`), since it's the same
      // underlying concept, just applied to a point instead of a path
      // boundary — the point marker itself sits at the bucket's center.
      map.addSource('pal-uncertainty', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'pal-uncertainty-line', type: 'line', source: 'pal-uncertainty',
        paint: {
          'line-color':     '#0088bb',
          'line-width':     5,
          'line-opacity':   0.95,
          'line-dasharray': [1, 0.5],
        },
      });

      // ── Encode mode: live waypoint-route preview ─────────────────────────
      map.addSource('encode-route', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'encode-route-casing', type: 'line', source: 'encode-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#0a0a14', 'line-width': 7, 'line-opacity': 0.6 },
      });
      map.addLayer({
        id: 'encode-route-line', type: 'line', source: 'encode-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#33ddaa', 'line-width': 4, 'line-opacity': 0.9 },
      });

      // Candidate-choice route preview: while the snap-candidate popup is
      // open, redrawn (a real routed polyline, not a straight ghost line)
      // every time a different candidate is selected, so you can compare
      // how the route actually changes before committing to one with
      // Enter. Dashed + amber to read as "not yet committed", distinct from
      // the solid teal committed route underneath.
      map.addSource('encode-preview-route', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'encode-preview-route-casing', type: 'line', source: 'encode-preview-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#0a0a14', 'line-width': 6, 'line-opacity': 0.7 },
      });
      map.addLayer({
        id: 'encode-preview-route-line', type: 'line', source: 'encode-preview-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#ffaa33', 'line-width': 3.5, 'line-opacity': 0.95, 'line-dasharray': [2, 1.5] },
      });

      // Live-stretch preview during a waypoint drag or a drag-to-insert
      // gesture — straight dashed line, real routing recomputes on release.
      map.addSource('encode-ghost', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'encode-ghost-line', type: 'line', source: 'encode-ghost',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#ffffff', 'line-width': 2.5, 'line-dasharray': [2, 2], 'line-opacity': 0.85 },
      });

      // Waypoint → snapped-node "offset" stubs: a waypoint is not an LRP —
      // it's just where the encoder starts looking. This makes the gap
      // between your click and where the encoder actually anchors visible,
      // color-coded by whether that gap is recoverable: amber for the
      // overall start/end (the true position survives as a POFF/NOFF
      // offset) vs red for an interior via-point (OpenLR's Line format has
      // no offset field on interior LRPs, so that precision is genuinely
      // not stored — only the snapped node is).
      map.addSource('encode-offset-stubs', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'encode-offset-stubs-line', type: 'line', source: 'encode-offset-stubs',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: {
          'line-width': 2.5,
          'line-dasharray': [1, 1.5],
          'line-opacity': 0.9,
          'line-color': ['match', ['get', 'category'], 'boundary', '#ffaa33', 'via', '#ff4455', '#ffaa33'],
        },
      });

      // Snap-candidate markers, shown while the disambiguation picker is
      // open: every nearby option as a small dot (blue = intersection, grey
      // = point along a road), plus a larger ring around whichever option
      // is currently *selected* (click only — see selectCandidate below).
      map.addSource('encode-snap-candidates', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'encode-snap-candidates-circle', type: 'circle', source: 'encode-snap-candidates',
        paint: {
          'circle-radius': 5,
          'circle-color': ['match', ['get', 'kind'], 'node', '#3399ff', '#aaaaaa'],
          'circle-stroke-color': '#0a0a14',
          'circle-stroke-width': 1.5,
        },
      });
      map.addSource('encode-snap-candidate-active', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'encode-snap-candidate-active-ring', type: 'circle', source: 'encode-snap-candidate-active',
        paint: {
          'circle-radius': 9,
          'circle-color': 'transparent',
          'circle-stroke-color': '#33ddaa',
          'circle-stroke-width': 3,
        },
      });
      // The raw point the user actually right-clicked/released at — distinct
      // from every snap candidate (which are all *options for where it might
      // anchor instead*), so it's never ambiguous which dot is your actual
      // click versus a suggested snap target.
      map.addSource('encode-click-point', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'encode-click-point-circle', type: 'circle', source: 'encode-click-point',
        paint: {
          'circle-radius': 6,
          'circle-color': '#ff3366',
          'circle-stroke-color': '#ffffff',
          'circle-stroke-width': 2,
        },
      });

      // ── Encode mode: right-click(-drag) waypoint editing ─────────────────
      // The right mouse button is dedicated entirely to placing/moving/
      // inserting waypoints — left click/drag stays plain map panning
      // (dragRotate is disabled and the native context menu suppressed
      // above specifically to free this button up). mousedown hit-tests, in
      // priority order, an existing marker (handled by the marker's own
      // listener below, which calls startEncodeDragRef), the route line
      // (insert a via-point), or else empty map (append — or for
      // PointAlongLine, replace the one point). Dragging is optional: a
      // right-click with zero movement and a right-click-drag both end the
      // same way — mouseup always opens the snap-candidate popup at the
      // release point, so this is a deliberate, precise action rather than
      // a low-friction default.
      function updateEncodeGhost(prevWp, nextWp, cursor) {
        const coords = [];
        if (prevWp) coords.push([prevWp.lon, prevWp.lat]);
        coords.push(cursor);
        if (nextWp) coords.push([nextWp.lon, nextWp.lat]);
        map.getSource('encode-ghost')?.setData({ type: 'Feature', geometry: { type: 'LineString', coordinates: coords }, properties: {} });
      }
      function clearEncodeGhost() {
        map.getSource('encode-ghost')?.setData({ type: 'FeatureCollection', features: [] });
      }
      let dragState = null; // { kind: 'move'|'insert'|'add', index }
      function onEncodeDragMove(e) {
        if (!dragState) return;
        const wps = useStore.getState().waypoints;
        const cursor = [e.lngLat.lng, e.lngLat.lat];
        if (dragState.kind === 'move') updateEncodeGhost(wps[dragState.index - 1], wps[dragState.index + 1], cursor);
        else if (dragState.kind === 'insert') updateEncodeGhost(wps[dragState.index - 1], wps[dragState.index], cursor);
        else if (dragState.kind === 'add' && wps.length > 0) updateEncodeGhost(wps[wps.length - 1], null, cursor);
      }
      function endEncodeDrag() {
        map.dragPan.enable();
        map.getCanvas().style.cursor = 'crosshair';
        map.off('mousemove', onEncodeDragMove);
        map.off('mouseup', onEncodeDragUp);
        document.removeEventListener('keydown', onEncodeDragKeyDown);
        clearEncodeGhost();
        dragState = null;
      }
      function onEncodeDragUp(e) {
        if (!dragState) return;
        const { kind, index } = dragState;
        const lonLat = { lon: e.lngLat.lng, lat: e.lngLat.lat };
        endEncodeDrag();
        showWaypointPopup(lonLat.lon, lonLat.lat, kind, index);
      }
      function onEncodeDragKeyDown(e) {
        if (e.key === 'Escape' && dragState) endEncodeDrag();
      }
      // Shared by the map-level mousedown handler below (insert/add) and the
      // waypoint marker effect's own per-marker mousedown listener (move).
      function startEncodeDrag(kind, index, e) {
        const waypoints = useStore.getState().waypoints;
        dragState = { kind, index };
        suppressNextClickRef.current = true;
        map.dragPan.disable();
        map.getCanvas().style.cursor = 'grabbing';
        const cursor = [e.lngLat.lng, e.lngLat.lat];
        if (kind === 'move') updateEncodeGhost(waypoints[index - 1], waypoints[index + 1], cursor);
        else if (kind === 'insert') updateEncodeGhost(waypoints[index - 1], waypoints[index], cursor);
        else if (waypoints.length > 0) updateEncodeGhost(waypoints[waypoints.length - 1], null, cursor);
        map.on('mousemove', onEncodeDragMove);
        map.on('mouseup', onEncodeDragUp);
        document.addEventListener('keydown', onEncodeDragKeyDown);
      }
      startEncodeDragRef.current = startEncodeDrag;

      function onEncodeMouseDown(e) {
        if (!encodeModeRef.current) return;
        if (e.originalEvent.button !== 2) return; // right button only
        e.preventDefault();
        const waypoints = useStore.getState().waypoints;
        const locType = useStore.getState().locationType;
        if (locType !== 'PointAlongLine' && waypoints.length >= 2) {
          const buf = 6;
          const hits = map.queryRenderedFeatures(
            [[e.point.x - buf, e.point.y - buf], [e.point.x + buf, e.point.y + buf]],
            { layers: ['encode-route-line', 'encode-route-casing'] }
          );
          if (hits.length) {
            const routeGeometry = useStore.getState().liveRoute?.geometry;
            const index = nearestWaypointPairIndex(waypoints, e.lngLat.lng, e.lngLat.lat, routeGeometry);
            startEncodeDrag('insert', index, e);
            return;
          }
        }
        startEncodeDrag('add', undefined, e);
      }
      map.on('mousedown', onEncodeMouseDown);

      // ── Click handlers ────────────────────────────────────────────────────
      const pointerOn  = () => { if (!measuringRef.current && !bearingActiveRef.current && !coordCaptureActiveRef.current) map.getCanvas().style.cursor = 'pointer'; };
      const pointerOff = () => {
        if (coordCaptureActiveRef.current) map.getCanvas().style.cursor = 'crosshair';
        else if (!measuringRef.current && !bearingActiveRef.current) map.getCanvas().style.cursor = '';
      };

      for (let frc = 0; frc < 8; frc++) {
        map.on('click', `olr-frc${frc}`, onSegmentClick);
        map.on('mouseenter', `olr-frc${frc}`, pointerOn);
        map.on('mouseleave', `olr-frc${frc}`, pointerOff);
      }

      map.on('click',      'olr-nodes-circle', onNodeClick);
      map.on('mouseenter', 'olr-nodes-circle', pointerOn);
      map.on('mouseleave', 'olr-nodes-circle', pointerOff);

      map.on('click', 'lrp-markers-circle', onLrpClick);
      map.on('mouseenter', 'lrp-markers-circle', pointerOn);
      map.on('mouseleave', 'lrp-markers-circle', pointerOff);

      map.on('mouseenter', 'replay-candidates-line',  pointerOn);
      map.on('mouseleave', 'replay-candidates-line',  pointerOff);

      map.on('click', 'decoded-path-line', onDecodedPathClick);
      map.on('mouseenter', 'decoded-path-line', pointerOn);
      map.on('mouseleave', 'decoded-path-line', pointerOff);

      map.on('click', onMapClick);
      map.on('mousemove', e => { const c = [e.lngLat.lng, e.lngLat.lat]; setCursorCoord(c); cursorCoordRef.current = c; });
      map.getCanvas().addEventListener('mouseleave', () => { setCursorCoord(null); cursorCoordRef.current = null; });

      loadVisibleTiles(map);
    });

    map.on('moveend', () => loadVisibleTiles(map));
    map.on('zoomend', () => loadVisibleTiles(map));

    // Resize the map whenever its container changes size (panel open/close).
    // Debounce so the WebGL canvas only resets once after a CSS transition
    // completes — resizing on every animation frame during a width transition
    // causes one blank frame per resize call, which is visible as flicker.
    let resizeTimer = null;
    const resizeObs = new ResizeObserver(() => {
      if (resizeTimer) clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => map.resize(), 220);
    });
    resizeObs.observe(mapContainer.current);

    return () => {
      if (resizeTimer) clearTimeout(resizeTimer);
      resizeObs.disconnect();
      if (pulseRef.current)         { cancelAnimationFrame(pulseRef.current);         pulseRef.current         = null; }
      if (frontierPulseRef.current) { cancelAnimationFrame(frontierPulseRef.current); frontierPulseRef.current = null; }
      if (routePulseRef.current)    { cancelAnimationFrame(routePulseRef.current);    routePulseRef.current    = null; }
      if (flashAnimRef.current)     { cancelAnimationFrame(flashAnimRef.current);     flashAnimRef.current     = null; }
      if (palPulseRef.current)      { cancelAnimationFrame(palPulseRef.current);      palPulseRef.current      = null; }
      map.remove();
      mapRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── Fly to the configured PMTiles archive's bounds ───────────────────────────
  // `initialBounds` starts `null` and is set at most once, after App.jsx's
  // startup effect resolves `pmtiles.getHeader()` (or stays `null` forever if
  // that lookup fails or returns a degenerate box — the map just keeps its
  // world-view default from the constructor above). Skipped if the page
  // loaded with an explicit #hash (a bookmark/share link naming a specific
  // view) or if the user already started panning/zooming before this fired.
  useEffect(() => {
    if (!initialBounds) return;
    if (initialHashRef.current) return;
    if (userInteractedRef.current) return;
    mapRef.current?.fitBounds(initialBounds, { padding: 40, duration: 0 });
  }, [initialBounds]);

  // ── Basemap switch ───────────────────────────────────────────────────────────

  function handleBasemapChange(id) {
    const map = mapRef.current;
    const entry = BASEMAPS.find(b => b.id === id);
    if (!map || !entry) return;
    map.setStyle(entry.style, {
      transformStyle: (previous, next) => ({
        ...next,
        sources: {
          ...next.sources,
          ...Object.fromEntries(
            Object.entries(previous.sources ?? {}).filter(([k]) => CUSTOM_SOURCES.has(k))
          ),
        },
        layers: [
          ...next.layers,
          ...(previous.layers ?? []).filter(l => CUSTOM_LAYER_IDS.has(l.id)),
        ],
      }),
    });
    setBasemap(id);
  }

  // ── Tile loading ─────────────────────────────────────────────────────────────

  async function loadVisibleTiles(map) {
    if (!map.isStyleLoaded()) return;
    const zoom = map.getZoom();
    if (zoom < MIN_LOAD_ZOOM) {
      setStatus(`Zoom in past ${MIN_LOAD_ZOOM} to load road segments`);
      return;
    }
    setStatus(null);

    // Ensure we have a PMTiles reader for the tile inspector.
    // We create a separate reader here (the decoder in store.js uses its own instance).
    // Both instances share the same underlying HTTP cache via the browser.
    if (!pmtilesRef.current) {
      try {
        const manifest = await fetch(`${tilesBaseRef.current}/manifest.json`).then(r => r.json());
        pmtilesRef.current = new PMTiles(`${tilesBaseRef.current}/${manifest.archive}`);
      } catch {
        return;
      }
    }

    const tileCache = tileCacheRef.current;
    const tiles   = tilesForBounds(map.getBounds(), TILE_ZOOM);
    const missing = tiles.filter(({ z, x, y }) => !tileCache.has(`${z}/${x}/${y}`));
    if (missing.length === 0) { rebuildSource(map, tiles); return; }

    pendingCountRef.current += missing.length;
    setStatus(`Loading ${pendingCountRef.current} tile${pendingCountRef.current > 1 ? 's' : ''}…`);

    await Promise.all(missing.map(async ({ z, x, y }) => {
      const key = `${z}/${x}/${y}`;
      try {
        const result = await pmtilesRef.current.getZxy(z, x, y);
        if (result?.data) {
          const fc = decodeTile(result.data, z, x, y);
          tileCache.set(key, fc.features);
          nodesCacheRef.current.set(key, fc.nodeFeatures ?? []);
        } else {
          tileCache.set(key, []);
          nodesCacheRef.current.set(key, []);
        }
      } catch (e) {
        console.error(`Tile ${key} failed:`, e);
        tileCache.set(key, []);
      } finally {
        pendingCountRef.current = Math.max(0, pendingCountRef.current - 1);
        if (pendingCountRef.current === 0) setStatus(null);
      }
    }));

    rebuildSource(map, tiles);
  }

  function rebuildSource(map, visibleTiles) {
    if (!map.getSource('olr-segments')) return;
    const visibleKeys = new Set(visibleTiles.map(({ z, x, y }) => `${z}/${x}/${y}`));
    const features = [];
    for (const [key, feats] of tileCacheRef.current) {
      if (visibleKeys.has(key)) features.push(...feats);
    }
    map.getSource('olr-segments').setData({ type: 'FeatureCollection', features });

    if (map.getSource('olr-nodes')) {
      const nodeFeatures = [];
      for (const [key, nFeats] of nodesCacheRef.current) {
        if (visibleKeys.has(key)) nodeFeatures.push(...nFeats);
      }
      map.getSource('olr-nodes').setData({ type: 'FeatureCollection', features: nodeFeatures });
    }
  }

  // ── Click interaction ────────────────────────────────────────────────────────

  function onNodeClick(e) {
    if (coordCaptureActiveRef.current) {
      cursorCoordRef.current = [e.lngLat.lng, e.lngLat.lat];
      commitCoordCapture();
      e.originalEvent.stopPropagation();
      return;
    }
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (!e.features?.length) return;
    const props = e.features[0].properties;
    const [z, x, y] = props.tile.split('/').map(Number);
    const nodeId = getNodeId(z, x, y, props.local_index);
    closeAllPopups();
    setNodeInfo({ ...props, node_id: nodeId >= 0 ? nodeId : null });
    setNodeAnchor({ x: e.point.x, y: e.point.y });
    e.originalEvent.stopPropagation();
  }

  function onSegmentClick(e) {
    if (coordCaptureActiveRef.current) {
      cursorCoordRef.current = [e.lngLat.lng, e.lngLat.lat];
      commitCoordCapture();
      e.originalEvent.stopPropagation();
      return;
    }
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (!e.features?.length) return;

    // When multiple features overlap near a segment boundary, pick the one whose
    // polyline geometry is closest to the click point in PIXEL space.  Geographic
    // distance fails at shared endpoints: both segments are equidistant there, so
    // whichever MapLibre returns first wins.  Pixel space matches what the user sees.
    const map = mapRef.current;
    const cp = e.point;                  // {x, y} pixels
    let bestFeat = e.features[0];
    let bestDist = Infinity;
    for (const feat of e.features) {
      const coords = feat.geometry?.coordinates;
      if (!coords?.length) continue;
      let minD = Infinity;
      for (let i = 0; i < coords.length - 1; i++) {
        const ap = map.project(coords[i]);
        const bp = map.project(coords[i + 1]);
        const dx = bp.x - ap.x, dy = bp.y - ap.y;
        const len2 = dx * dx + dy * dy;
        const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((cp.x - ap.x) * dx + (cp.y - ap.y) * dy) / len2));
        const ex = cp.x - (ap.x + t * dx), ey = cp.y - (ap.y + t * dy);
        const d = ex * ex + ey * ey;
        if (d < minD) minD = d;
      }
      if (minD < bestDist) { bestDist = minD; bestFeat = feat; }
    }
    const props = bestFeat.properties;
    const [z, x, y] = props.tile.split('/').map(Number);
    const segId = getSegmentId(z, x, y, props.local_index);
    const segCoords = bestFeat.geometry?.coordinates;
    if (segCoords?.length) {
      pendingPopupCoordRef.current = polylineMid(segCoords);
    }
    closeAllPopups();
    setHighlightedSegment({ tile: props.tile, local_index: props.local_index });
    setInfoProps({ ...props, segment_id: segId >= 0 ? segId : null });
    e.originalEvent.stopPropagation();
  }

  function onDecodedPathClick(e) {
    if (coordCaptureActiveRef.current) {
      cursorCoordRef.current = [e.lngLat.lng, e.lngLat.lat];
      commitCoordCapture();
      e.originalEvent.stopPropagation();
      return;
    }
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    e.originalEvent.stopPropagation();
    const segments = decodeResultRef.current?.segments;
    if (!segments?.length) return;

    const map = mapRef.current;
    const cp = e.point;
    const cache = getSegGeomCache();
    let best = null, bestDist = Infinity;

    for (const s of segments) {
      const [z, x, y] = s.tile.split('/').map(Number);
      const segId = getSegmentId(z, x, y, s.local_index);
      const feat = segId >= 0 ? cache.get(segId) : null;
      if (!feat) continue;
      const coords = feat.geometry.coordinates;
      let minD = Infinity;
      for (let i = 0; i < coords.length - 1; i++) {
        const ap = map.project(coords[i]);
        const bp = map.project(coords[i + 1]);
        const dx = bp.x - ap.x, dy = bp.y - ap.y;
        const len2 = dx * dx + dy * dy;
        const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((cp.x - ap.x) * dx + (cp.y - ap.y) * dy) / len2));
        const ex = cp.x - (ap.x + t * dx), ey = cp.y - (ap.y + t * dy);
        const d = ex * ex + ey * ey;
        if (d < minD) minD = d;
      }
      if (minD < bestDist) { bestDist = minD; best = { feat, segId }; }
    }

    if (best) {
      const bestCoords = best.feat.geometry?.coordinates;
      if (bestCoords?.length) {
        pendingPopupCoordRef.current = polylineMid(bestCoords);
      }
      closeAllPopups();
      setInfoProps({ ...best.feat.properties, segment_id: best.segId });
      setHighlightedSegment({
        tile:        best.feat.properties.tile,
        local_index: best.feat.properties.local_index,
      });
    }
  }

  function onLrpClick(e) {
    if (coordCaptureActiveRef.current) {
      cursorCoordRef.current = [e.lngLat.lng, e.lngLat.lat];
      commitCoordCapture();
      e.stopPropagation();
      e.originalEvent.stopPropagation();
      return;
    }
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (!e.features?.length) return;
    closeAllPopups();
    setLrpInfo(e.features[0].properties);
    setInfoAnchor({ x: e.point.x, y: e.point.y });
    setHighlightedSegment(null);
    e.stopPropagation();           // stop lower-Z layers (segments) from also firing
    e.originalEvent.stopPropagation();
  }

  // The one popup shown after every right-click(-drag) waypoint edit —
  // lets the user pick precisely which nearby road/intersection to snap
  // onto (rather than silently choosing the nearest), see the exact
  // coordinate, and — for Line — optionally mark this as the last waypoint
  // to jump straight to the Results panel, or — for PointAlongLine — set
  // orientation/side-of-road right here since there's only ever one click.
  //
  // `kind` is 'add' | 'insert' | 'move'; `index` is the via-point/waypoint
  // index for 'insert'/'move' (unused for 'add').
  async function showWaypointPopup(lon, lat, kind, index) {
    const map = mapRef.current;
    if (!map) return;
    if (snapPickerPopupRef.current) { snapPickerPopupRef.current.remove(); snapPickerPopupRef.current = null; }

    // Candidates come from the *encoder's* own loaded graph, which (unlike
    // the always-on decode tile set) only has tiles fetched for areas
    // already touched by a committed waypoint — a fresh area right-clicked
    // for the first time would otherwise show a false "no roads found"
    // just because nothing has loaded there yet, not because there's
    // nothing there. Load first, then check.
    await loadEncoderTilesNear(lon, lat);
    if (mapRef.current !== map) return; // map was torn down while awaiting

    const isPal = useStore.getState().locationType === 'PointAlongLine';
    const candidates = getSnapCandidates(lon, lat);

    const clearSnapHighlights = () => {
      map.getSource('encode-snap-candidates')?.setData({ type: 'FeatureCollection', features: [] });
      map.getSource('encode-snap-candidate-active')?.setData({ type: 'FeatureCollection', features: [] });
      map.getSource('encode-click-point')?.setData({ type: 'FeatureCollection', features: [] });
    };
    const setActiveHighlight = (c) => {
      if (!c) { map.getSource('encode-snap-candidate-active')?.setData({ type: 'FeatureCollection', features: [] }); return; }
      map.getSource('encode-snap-candidate-active')?.setData({
        type: 'Feature', geometry: { type: 'Point', coordinates: [c.snapped_lon, c.snapped_lat] }, properties: {},
      });
    };
    // Show every candidate as a small dot immediately, so it's clear at a
    // glance where each option actually is before selecting any of them.
    map.getSource('encode-snap-candidates')?.setData({
      type: 'FeatureCollection',
      features: candidates.map(c => ({
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [c.snapped_lon, c.snapped_lat] },
        properties: { kind: c.kind },
      })),
    });
    // The raw click point itself, visually distinct from every candidate —
    // see the layer's own comment above for why.
    map.getSource('encode-click-point')?.setData({
      type: 'Feature', geometry: { type: 'Point', coordinates: [lon, lat] }, properties: {},
    });

    let activeCandidate = candidates[0] ?? null;
    setActiveHighlight(activeCandidate);
    // Assigned below when building the actions row (Line only) — declared
    // here so `commit()`'s closure can read whatever it ends up being.
    let lastWaypointCheckbox = null;

    const pointFromCandidate = (c) => c?.kind === 'node'
      ? { lon, lat, node_id: c.node_id }
      : c?.kind === 'segment'
        ? { lon, lat, segment_id: c.segment_id }
        : { lon, lat };

    // The waypoint list this popup would produce for a given point choice,
    // without touching real store state — same splice logic addWaypoint/
    // insertWaypoint/moveWaypoint apply, computed here purely to drive the
    // route preview below.
    const currentWaypoints = useStore.getState().waypoints;
    const hypotheticalWaypoints = (point) => {
      if (kind === 'insert') return [...currentWaypoints.slice(0, index), point, ...currentWaypoints.slice(index)];
      if (kind === 'move')   return currentWaypoints.map((w, i) => (i === index ? point : w));
      return [...currentWaypoints, point];
    };
    // A single point (PAL, or the very first waypoint of a fresh Line) has
    // no route to draw at all — Rule-1's "at least 2 waypoints" floor.
    const hypotheticalCount = kind === 'insert' || kind === 'move' ? currentWaypoints.length : currentWaypoints.length + 1;

    const clearPreviewRoute = () => map.getSource('encode-preview-route')?.setData({ type: 'FeatureCollection', features: [] });

    // Open the popup on whichever side of the click point has the *least*
    // stuff to cover — both the adjoining waypoint(s) (the already-drawn
    // approach) and every candidate's snapped position (wherever the
    // preview route extends to, which moves as the selection changes — see
    // previewFor below). Averaging both in means the popup can't perfectly
    // dodge every possible selection when candidates scatter in different
    // directions, but it beats only accounting for the incoming route and
    // then covering the very cluster of candidates/preview it's there to
    // let you compare.
    const neighborWaypoints = kind === 'add'
      ? (currentWaypoints.length ? [currentWaypoints[currentWaypoints.length - 1]] : [])
      : kind === 'move'
        ? [currentWaypoints[index - 1], currentWaypoints[index + 1]].filter(Boolean)
        : [currentWaypoints[index - 1], currentWaypoints[index]].filter(Boolean); // 'insert': the pair it splits
    const avoidPoints = [
      ...neighborWaypoints.map(n => ({ lon: n.lon, lat: n.lat })),
      ...candidates.map(c => ({ lon: c.snapped_lon, lat: c.snapped_lat })),
    ];
    let popupAnchor;
    if (avoidPoints.length) {
      let dx = 0, dy = 0;
      for (const p of avoidPoints) { dx += p.lon - lon; dy += p.lat - lat; }
      dx /= avoidPoints.length; dy /= avoidPoints.length;
      const preferred = Math.abs(dx) > Math.abs(dy)
        ? (dx > 0 ? 'right' : 'left')   // stuff-to-avoid is east/west → open on the opposite side
        : (dy > 0 ? 'top' : 'bottom');  // stuff-to-avoid is north/south → open on the opposite side

      // A forced anchor disables MapLibre's own built-in viewport clamping
      // (it only auto-keeps the popup on-screen when `anchor` is left
      // unset) — so if the click is close enough to that edge of the map
      // that the popup would run off-screen anyway, don't force it: fall
      // back to auto-placement, which stays on-screen even if that means
      // covering the route in this edge case.
      const EST_W = 340, EST_H = 420; // generous estimate incl. candidate list + PAL selects
      const p = map.project([lon, lat]);
      const container = map.getContainer();
      const fits = {
        left:   p.x + EST_W <= container.clientWidth,   // popup extends right
        right:  p.x - EST_W >= 0,                       // popup extends left
        top:    p.y + EST_H <= container.clientHeight,  // popup extends down
        bottom: p.y - EST_H >= 0,                       // popup extends up
      };
      if (fits[preferred]) popupAnchor = preferred;
    }

    // Redraws the *actual* routed geometry for choosing `c` — not a straight
    // ghost line — so clicking through candidates lets you compare how the
    // real route differs for each before committing one with Enter.
    let previewToken = 0;
    const previewFor = async (c) => {
      if (isPal || hypotheticalWaypoints(pointFromCandidate(c)).length < 2) { clearPreviewRoute(); return; }
      const myToken = ++previewToken; // guards against a slow earlier preview overwriting a later one
      const maxTurnDeviationDeg = useStore.getState().params.max_interior_turn_deviation_deg;
      const result = await previewRouteBetween(hypotheticalWaypoints(pointFromCandidate(c)), maxTurnDeviationDeg);
      if (myToken !== previewToken || mapRef.current !== map) return;
      if (!result?.geometry?.length) { clearPreviewRoute(); return; }
      map.getSource('encode-preview-route')?.setData({
        type: 'Feature', geometry: { type: 'LineString', coordinates: result.geometry }, properties: {},
      });
    };

    // Commits `c` (a candidate, or null to fall back to plain lon/lat —
    // the encoder's own nearest-road search then applies) as the
    // add/insert/move action this popup was opened for. Awaits the store
    // action all the way through — including its own internal tile-load +
    // runLiveRoute() — so the caller can gate the Enter button's re-enable
    // on the highlighted route geometry actually having been updated, not
    // just on the call having been issued.
    const commitCandidate = async (c) => {
      const point = pointFromCandidate(c);
      if (kind === 'insert') await insertWaypoint(index, point);
      else if (kind === 'move') await moveWaypoint(index, point);
      else await addWaypoint(point);

      // PAL is implicitly always "last" (there's only ever one point); Line
      // only when the checkbox says so. Either way: open the Results panel
      // and kick off the actual encode immediately — not awaited, so the
      // popup can close right away and the panel shows the in-progress/
      // finished result reactively instead of blocking Enter on the full
      // encode + verify round trip.
      if (isPal || lastWaypointCheckbox?.checked) {
        const store = useStore.getState();
        store.openResult();
        if (isPal) store.runEncodePal();
        else store.runEncode();
      }
    };

    const content = document.createElement('div');
    content.className = 'loc-pin-popup snap-picker-popup';

    const title = document.createElement('div');
    title.className = 'loc-pin-coord snap-picker-drag-handle';
    title.title = 'Drag to move this popup out of the way';
    const kindLabel = isPal ? 'Place point'
      : kind === 'insert' ? 'Insert waypoint'
      : kind === 'move'   ? 'Move waypoint'
      : 'Add waypoint';
    title.textContent = `${kindLabel} — ${lat.toFixed(6)}, ${lon.toFixed(6)}`;
    content.appendChild(title);

    if (candidates.length > 0) {
      const list = document.createElement('div');
      list.className = 'snap-picker-list';
      const selectCandidate = (c, btn) => {
        activeCandidate = c;
        setActiveHighlight(c);
        list.querySelectorAll('.snap-picker-option.selected').forEach(el => el.classList.remove('selected'));
        btn.classList.add('selected');
        previewFor(c);
      };
      candidates.forEach(c => {
        const btn = document.createElement('button');
        btn.className = 'snap-picker-option';
        const label = c.kind === 'node'
          ? 'Intersection'
          : (c.stable_id || `Segment #${c.segment_id}`);
        const detail = c.kind === 'node' ? '' : ` · FRC${c.frc}`;
        btn.textContent = `${label}${detail} · ${c.distance_m.toFixed(0)}m`;
        if (c === activeCandidate) btn.classList.add('selected');
        // Only a click changes the selection (and its route preview) — a
        // mere mouse-over must not, even though it's a common map-UI
        // convention elsewhere, since here it would make the highlighted
        // snap point (and the route preview computed for it) change
        // without the user having committed to anything.
        btn.addEventListener('click', () => selectCandidate(c, btn));
        list.appendChild(btn);
      });
      content.appendChild(list);
    } else {
      const none = document.createElement('div');
      none.className = 'snap-picker-none';
      none.textContent = 'No roads found very close by — will snap to the nearest available road.';
      content.appendChild(none);
    }

    // Preview whatever's selected by default (candidates[0], or the bare-
    // point fallback) immediately, before any click — so there's something
    // to compare the very first alternative against.
    previewFor(activeCandidate);

    if (isPal) {
      const store = useStore.getState();
      const options = document.createElement('div');
      options.className = 'snap-picker-options';

      const orientSelect = document.createElement('select');
      orientSelect.className = 'encode-select';
      for (const o of ['NoOrientation', 'FirstTowardSecond', 'SecondTowardFirst', 'BothDirections']) {
        const opt = document.createElement('option');
        opt.value = o; opt.textContent = o;
        if (o === store.palOrientation) opt.selected = true;
        orientSelect.appendChild(opt);
      }
      orientSelect.addEventListener('change', () => useStore.getState().setPalOrientation(orientSelect.value));

      const sorSelect = document.createElement('select');
      sorSelect.className = 'encode-select';
      for (const s of ['DirectlyOnOrNA', 'Right', 'Left', 'Both']) {
        const opt = document.createElement('option');
        opt.value = s; opt.textContent = s;
        if (s === store.palSideOfRoad) opt.selected = true;
        sorSelect.appendChild(opt);
      }
      sorSelect.addEventListener('change', () => useStore.getState().setPalSideOfRoad(sorSelect.value));

      const orientLabel = document.createElement('label');
      orientLabel.className = 'encode-select-label';
      orientLabel.textContent = 'Orientation';
      orientLabel.appendChild(orientSelect);
      const sorLabel = document.createElement('label');
      sorLabel.className = 'encode-select-label';
      sorLabel.textContent = 'Side of road';
      sorLabel.appendChild(sorSelect);

      options.appendChild(orientLabel);
      options.appendChild(sorLabel);
      content.appendChild(options);
    }

    const actions = document.createElement('div');
    actions.className = 'snap-picker-actions';

    if (!isPal) {
      const checkboxLabel = document.createElement('label');
      checkboxLabel.className = 'snap-picker-last-wp';
      lastWaypointCheckbox = document.createElement('input');
      lastWaypointCheckbox.type = 'checkbox';
      // A single waypoint has no route at all yet (Line needs at least 2) —
      // marking it "last" wouldn't produce anything encodable.
      lastWaypointCheckbox.disabled = hypotheticalCount < 2;
      if (lastWaypointCheckbox.disabled) checkboxLabel.title = 'Need at least 2 waypoints before this route can be encoded';
      checkboxLabel.appendChild(lastWaypointCheckbox);
      checkboxLabel.appendChild(document.createTextNode('Last waypoint'));
      actions.appendChild(checkboxLabel);
    }

    const enterBtn = document.createElement('button');
    enterBtn.className = 'snap-picker-enter';
    enterBtn.textContent = 'Enter';
    enterBtn.addEventListener('click', async () => {
      // Disabled for the full round trip (tile load + waypoint commit +
      // route regeneration), not just the initial call — re-enabling only
      // once the highlighted route geometry actually reflects the new
      // waypoint prevents a rapid double-click from committing it twice.
      enterBtn.disabled = true;
      enterBtn.textContent = 'Adding…';
      try {
        await commitCandidate(activeCandidate);
      } finally {
        enterBtn.disabled = false;
        enterBtn.textContent = 'Enter';
      }
      popup.remove(); // triggers the 'close' handler below, which clears the preview route too
    });
    actions.appendChild(enterBtn);

    const cancelBtn = document.createElement('button');
    cancelBtn.className = 'snap-picker-cancel';
    cancelBtn.textContent = 'Cancel';
    cancelBtn.addEventListener('click', () => popup.remove());
    actions.appendChild(cancelBtn);

    content.appendChild(actions);

    const popup = new maplibregl.Popup({
      closeButton: true, offset: 12, className: 'loc-pin-popup-wrap',
      maxWidth: '320px', anchor: popupAnchor,
    })
      .setLngLat([lon, lat])
      .setDOMContent(content)
      .addTo(map);
    popup.on('close', () => {
      clearSnapHighlights();
      clearPreviewRoute();
      previewToken++; // discard any still-in-flight preview request
      if (snapPickerPopupRef.current === popup) snapPickerPopupRef.current = null;
    });
    snapPickerPopupRef.current = popup;

    // Auto-placement can't guarantee the geometry it's there to help you
    // compare stays uncovered — the popup's tail is anchored right at the
    // click point, and candidates cluster within meters of it, so *some*
    // side will always be close to the route/candidates. Letting the user
    // drag it out of the way directly is the reliable fix. Uses `setOffset`
    // (not a raw CSS transform) so MapLibre's own position recomputation
    // — e.g. on pan/zoom — keeps applying it correctly instead of
    // overwriting it on the next re-render.
    //
    // MapLibre's `Popup` has `setOffset()` but, unlike `Marker`, no
    // `getOffset()` to read the current value back — so the offset is
    // tracked here instead of queried from the popup.
    let currentOffset = [0, 0];
    let dragStart = null; // { mouseX, mouseY }
    const onDragMove = (e) => {
      if (!dragStart) return;
      currentOffset = [
        dragStart.baseX + (e.clientX - dragStart.mouseX),
        dragStart.baseY + (e.clientY - dragStart.mouseY),
      ];
      popup.setOffset(currentOffset);
    };
    const onDragEnd = () => {
      dragStart = null;
      document.removeEventListener('mousemove', onDragMove);
      document.removeEventListener('mouseup', onDragEnd);
    };
    title.addEventListener('mousedown', (e) => {
      e.preventDefault(); // avoid text-selection while dragging
      dragStart = { mouseX: e.clientX, mouseY: e.clientY, baseX: currentOffset[0], baseY: currentOffset[1] };
      document.addEventListener('mousemove', onDragMove);
      document.addEventListener('mouseup', onDragEnd);
    });
  }

  function onMapClick(e) {
    if (coordCaptureActiveRef.current) {
      cursorCoordRef.current = [e.lngLat.lng, e.lngLat.lat];
      commitCoordCapture();
      return;
    }
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      return;
    }
    if (encodeModeRef.current) {
      // Left click does nothing in encode mode — waypoint editing is
      // exclusively right-click(-drag), handled by onEncodeMouseDown/
      // onEncodeDragUp above, so a plain left click just falls through to
      // the ordinary deselect/close-popups behavior below (consistent with
      // decode mode's empty-map click).
      if (suppressNextClickRef.current) { suppressNextClickRef.current = false; return; }
    }
    const layerIds = [...Array.from({ length: 8 }, (_, i) => `olr-frc${i}`), 'lrp-markers-circle', 'decoded-path-line'];
    const hits = mapRef.current.queryRenderedFeatures(e.point, { layers: layerIds });
    if (hits.length > 0) return;
    setHighlightedSegment(null);
    closeAllPopups();
  }

  // ── Highlight sync (store → map) ────────────────────────────────────────────
  // Depends only on highlightedSegment; reads decodeResult via ref so it never
  // races with the decode-result effect.

  useEffect(() => {
    const map = mapRef.current;

    if (pulseRef.current) { cancelAnimationFrame(pulseRef.current); pulseRef.current = null; }

    if (!map) return;

    const clearHighlight = () => {
      const src = map.getSource('highlighted-segment');
      if (src) src.setData({ type: 'FeatureCollection', features: [] });
      if (!traceHighlightSegIds?.length) {
        if (map.getLayer('olr-highlight')) map.setFilter('olr-highlight', ['boolean', false]);
      }
    };

    if (!highlightedSegment) { clearHighlight(); return; }

    // Look up geometry from the always-current ref (no dep needed)
    const seg = decodeResultRef.current?.segments?.find(
      s => s.tile === highlightedSegment.tile && s.local_index === highlightedSegment.local_index
    );

    if (seg?.geometry?.length >= 2) {
      const src = map.getSource('highlighted-segment');
      if (src) src.setData({
        type: 'FeatureCollection',
        features: [{ type: 'Feature', geometry: { type: 'LineString', coordinates: seg.geometry }, properties: {} }],
      });
      // Only clear tile-layer filter when trace panel isn't using it
      if (!traceHighlightSegIds?.length) {
        if (map.getLayer('olr-highlight')) map.setFilter('olr-highlight', ['boolean', false]);
      }

      // Fit map to the clicked segment's extent
      const lngs = seg.geometry.map(c => c[0]);
      const lats = seg.geometry.map(c => c[1]);
      map.fitBounds(
        [[Math.min(...lngs), Math.min(...lats)], [Math.max(...lngs), Math.max(...lats)]],
        { padding: 160, duration: 400, maxZoom: 18 },
      );

      // If a popup anchor is waiting, register the moveend listener HERE — after
      // fitBounds — so it can't be triggered by any prior map movement.
      if (pendingPopupCoordRef.current) {
        const coord = pendingPopupCoordRef.current;
        pendingPopupCoordRef.current = null;
        map.once('moveend', () => {
          const pt = map.project(coord);
          setInfoAnchor({ x: Math.max(pt.x, 20), y: pt.y });
        });
      }

      // Sinusoidal halo pulse
      const t0 = performance.now();
      const pulse = (now) => {
        if (!map.getLayer('highlighted-segment-halo')) return;
        const phase = ((now - t0) / 700) * Math.PI * 2;
        map.setPaintProperty('highlighted-segment-halo', 'line-opacity',
          0.25 + 0.5 * (0.5 + 0.5 * Math.sin(phase)));
        pulseRef.current = requestAnimationFrame(pulse);
      };
      pulseRef.current = requestAnimationFrame(pulse);
    } else {
      // Background segment click — olr-highlight filter (skip if trace panel owns the filter)
      const src = map.getSource('highlighted-segment');
      if (src) src.setData({ type: 'FeatureCollection', features: [] });
      if (!traceHighlightSegIds?.length && map.getLayer('olr-highlight')) {
        map.setFilter('olr-highlight', ['all',
          ['==', ['get', 'tile'],        highlightedSegment.tile],
          ['==', ['get', 'local_index'], highlightedSegment.local_index],
        ]);
      }
      // No fitBounds — project pending popup anchor immediately
      if (pendingPopupCoordRef.current) {
        const coord = pendingPopupCoordRef.current;
        pendingPopupCoordRef.current = null;
        const pt = map.project(coord);
        setInfoAnchor({ x: Math.max(pt.x, 20), y: pt.y });
      }
    }
  }, [highlightedSegment, traceHighlightSegIds]); // ← decodeResult excluded; read via ref

  // ── Trace highlight sync (trace panel → dedicated trace-segment layer) ───────
  // Uses the decode-time geometry cache so highlights work regardless of
  // whether those tiles are currently loaded in the display cache.

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    const traceSource = map.getSource('trace-segment');
    if (!traceSource) return;

    if (!traceHighlightSegIds?.length) {
      traceSource.setData({ type: 'FeatureCollection', features: [] });
      return;
    }

    const segGeomCache  = getSegGeomCache();
    const segIdToTile   = getSegIdToTile();
    const tileGeomCache = getTileGeomCache();
    const features      = [];
    const allCoords     = [];

    for (const segId of traceHighlightSegIds) {
      // Primary: direct segId → feature lookup (built at decode time)
      let feat = segGeomCache.get(segId);

      // Fallback: two-step lookup via segIdToTile + tileGeomCache
      if (!feat) {
        const mapping = segIdToTile.get(segId);
        if (mapping) {
          feat = tileGeomCache.get(mapping.tile_key)?.find(f => f.properties.local_index === mapping.local_index);
          if (feat) console.log('[trace-hl] two-step fallback hit segId', segId, 'mapping:', mapping);
        }
        if (!feat) {
          console.warn('[trace-hl] segId', segId, 'not found.',
            'segGeomCache.size:', segGeomCache.size,
            'mapping:', mapping,
            'tileKeys in tileGeomCache:', [...tileGeomCache.keys()].slice(0, 5));
          continue;
        }
      }
      features.push(feat);
      if (feat.geometry?.coordinates) allCoords.push(...feat.geometry.coordinates);
    }

    // Clip first/last segment at LRP snap points when highlighting a leg route.
    if (traceHighlightSnaps && features.length > 0) {
      const { from, to } = traceHighlightSnaps;
      if (from && features[0]?.geometry?.coordinates) {
        const coords = clipGeomFromPoint(features[0].geometry.coordinates, from[0], from[1]);
        if (coords) features[0] = { ...features[0], geometry: { type: 'LineString', coordinates: coords } };
      }
      const last = features.length - 1;
      if (to && features[last]?.geometry?.coordinates) {
        const coords = clipGeomToPoint(features[last].geometry.coordinates, to[0], to[1]);
        if (coords) features[last] = { ...features[last], geometry: { type: 'LineString', coordinates: coords } };
      }
    }

    // When a candidate popup is active for a Backward traversal, reverse the
    // coordinate order so trace-segment-arrow chevrons point the correct way.
    const cp = candidatePopupRef.current;
    if (
      cp?.traversal === 'Backward' &&
      features.length === 1 &&
      traceHighlightSegIds?.length === 1 &&
      traceHighlightSegIds[0] === cp.segment_id
    ) {
      const f = features[0];
      features[0] = {
        ...f,
        geometry: { type: 'LineString', coordinates: [...f.geometry.coordinates].reverse() },
      };
    }

    traceSource.setData({ type: 'FeatureCollection', features });

    // Pan to the bounding box of the highlighted segments
    if (allCoords.length >= 2) {
      const lngs = allCoords.map(c => c[0]);
      const lats = allCoords.map(c => c[1]);
      map.fitBounds(
        [[Math.min(...lngs), Math.min(...lats)], [Math.max(...lngs), Math.max(...lats)]],
        { padding: 160, duration: 400, maxZoom: 18 },
      );
    }

    // Consume the pending candidate anchor (set by candidatePopup effect) if
    // fitBounds was just called — project it in the same moveend callback.
    const pendingCandCoord = (allCoords.length >= 2) ? pendingCandAnchorCoordRef.current : null;
    if (pendingCandCoord) pendingCandAnchorCoordRef.current = null;

    // Show segment info popup for single-segment trace clicks, but not
    // when a candidate evaluation popup is already being shown.
    let moveEndHandler = null;

    if (features.length === 1 && !candidatePopupRef.current) {
      const feat = features[0];
      // segId is the WASM runtime segment_id — include it so the popup
      // doesn't show "— (decode first)" for Internal ID.
      closeAllPopups();
      setInfoProps({ ...feat.properties, segment_id: traceHighlightSegIds[0] });
      setInfoAnchor(null); // defer until fitBounds animation completes
      const coords = feat.geometry?.coordinates;
      if (coords?.length && allCoords.length >= 2) {
        const mid = polylineMid(coords);
        moveEndHandler = () => {
          const pixel = map.project(mid);
          setInfoAnchor({ x: Math.max(pixel.x, 20), y: pixel.y });
          if (pendingCandCoord) {
            const pt = map.project(pendingCandCoord);
            setCandAnchor({ x: pt.x, y: pt.y });
          }
        };
      } else if (coords?.length) {
        // No fitBounds — project immediately
        const pixel = map.project(polylineMid(coords));
        setInfoAnchor({ x: Math.max(pixel.x, 20), y: pixel.y });
      }
    } else if (pendingCandCoord) {
      // Candidate popup open, no segment info popup — just project the cand anchor
      moveEndHandler = () => {
        const pt = map.project(pendingCandCoord);
        setCandAnchor({ x: pt.x, y: pt.y });
      };
    }

    if (moveEndHandler) {
      map.once('moveend', moveEndHandler);
      return () => map.off('moveend', moveEndHandler);
    }
  }, [traceHighlightSegIds, traceHighlightSnaps]);

  // ── Trace panel LRP focus (pan + popup) ─────────────────────────────────────

  useEffect(() => {
    if (!traceLrpFocus) return;
    const map = mapRef.current;
    if (!map) return;

    const { lon, lat, index, frc, fow, lfrcnp, bearing_lb, bearing_ub } = traceLrpFocus;
    map.flyTo({ center: [lon, lat], zoom: Math.max(map.getZoom(), 15), duration: 500 });
    // Enrich with snap info from decodeResult.lrps if available
    const lrpData = decodeResult?.lrps?.[index] ?? {};
    closeAllPopups();
    setLrpInfo({
      index, lat, lon, frc, fow, lfrcnp: lfrcnp ?? null, bearing_lb, bearing_ub,
      snap_lon: lrpData.snap_lon ?? null,
      snap_lat: lrpData.snap_lat ?? null,
      snap_is_endpoint: lrpData.snap_is_endpoint ?? null,
      snap_distance_m: lrpData.snap_distance_m ?? null,
    });
    // Position popup near map center (LRP will fly there)
    setInfoAnchor({ x: map.getCanvas().clientWidth / 2, y: map.getCanvas().clientHeight / 2 });
    // Allow re-clicking same LRP by clearing after acting
    setTraceLrpFocus(null);
  }, [traceLrpFocus, setTraceLrpFocus]);

  // ── LLM-requested map fly-to ─────────────────────────────────────────────────

  useEffect(() => {
    if (!mapFlyTo) return;
    const map = mapRef.current;
    if (!map) return;
    map.flyTo({ center: [mapFlyTo.lon, mapFlyTo.lat], zoom: mapFlyTo.zoom, duration: 700 });
  }, [mapFlyTo]);

  // ── LRP bearing cone sync ─────────────────────────────────────────────────────

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;
    const src = map.getSource('lrp-bearing');
    if (!src) return;
    if (!lrpInfo) { src.setData({ type: 'FeatureCollection', features: [] }); return; }
    const { lon, lat, snap_lon, snap_lat, bearing_lb, bearing_ub } = lrpInfo;
    const coneLon = snap_lon ?? lon;
    const coneLat = snap_lat ?? lat;
    src.setData(bearingConeGeoJSON(coneLon, coneLat, bearing_lb, bearing_ub, searchRadiusM ?? 100));
  }, [lrpInfo, searchRadiusM]);

  // ── Replay visual effect ─────────────────────────────────────────────────────

  const replayLayerIds = [
    'replay-radius-fill', 'replay-radius-line',
    'replay-traversed-line',
    'replay-candidates-line', 'replay-candidates-arrow',
    'replay-cloud-circle',
    'replay-frontier-circle',
    'replay-leg-from', 'replay-leg-to',
    'replay-flash-ring',
  ];

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    if (frontierPulseRef.current) {
      cancelAnimationFrame(frontierPulseRef.current);
      frontierPulseRef.current = null;
    }
    if (flashAnimRef.current) {
      cancelAnimationFrame(flashAnimRef.current);
      flashAnimRef.current = null;
    }
    if (routePulseRef.current) {
      cancelAnimationFrame(routePulseRef.current);
      routePulseRef.current = null;
    }
    if (decodePulseRef.current) {
      cancelAnimationFrame(decodePulseRef.current);
      decodePulseRef.current = null;
    }

    const emptyFC = { type: 'FeatureCollection', features: [] };
    const replaySources = ['replay-radius', 'replay-route', 'replay-traversed', 'replay-candidates', 'replay-cloud', 'replay-frontier', 'replay-leg', 'replay-flash'];
    const vis = showReplay && replaySteps.length > 0 ? 'visible' : 'none';
    // Arrow layer is managed separately (shown only when a candidate is selected).
    replayLayerIds.forEach(id => {
      if (id === 'replay-candidates-arrow') return;
      if (map.getLayer(id)) map.setLayoutProperty(id, 'visibility', vis);
    });
    if (map.getLayer('replay-candidates-arrow'))
      map.setLayoutProperty('replay-candidates-arrow', 'visibility', 'none');
    // Hide the full decoded path while replay is active so the per-leg highlight is unambiguous.
    if (map.getLayer('decoded-path-line')) {
      map.setLayoutProperty('decoded-path-line', 'visibility', vis === 'visible' ? 'none' : 'visible');
    }

    if (!showReplay || !replaySteps.length) {
      replaySources.forEach(s => { map.getSource(s)?.setData(emptyFC); });
      replayVisualRef.current = null;
      replayStepRef.current   = -1;
      replayStepsKey.current  = null;
      return;
    }

    const maxG = replayStats?.maxG ?? 0;

    // Reset incremental state when a new decode's steps arrive
    if (replayStepsKey.current !== replaySteps) {
      replayVisualRef.current = null;
      replayStepRef.current   = -1;
      replayStepsKey.current  = replaySteps;
    }

    // ── Incremental update ──────────────────────────────────────────────────
    // Forward step: apply only the new step(s) onto the existing state (O(1)).
    // Backward / jump: recompute from scratch (O(N)).
    let visualState;
    if (replayVisualRef.current && replayStep >= replayStepRef.current) {
      // Clone once, then apply each new step in place
      visualState = replayVisualRef.current;
      for (let i = replayStepRef.current + 1; i <= replayStep; i++) {
        applyStep(visualState, replaySteps[i], maxG);
        visualState.stepIdx = i;
      }
    } else {
      visualState = computeVisualState(replaySteps, replayStep, replayStats);
    }
    replayVisualRef.current = visualState;
    replayStepRef.current   = replayStep;

    // ── Push GeoJSON to sources ─────────────────────────────────────────────
    const gj = stateToGeoJSON(visualState, id => getSegGeomCache().get(id));
    map.getSource('replay-radius')            ?.setData(gj.radiusFC);
    map.getSource('replay-candidates')        ?.setData(gj.candFC);
    map.getSource('replay-traversed')  ?.setData(gj.traversedFC);
    map.getSource('replay-cloud')      ?.setData(gj.cloudFC);
    map.getSource('replay-frontier')   ?.setData(gj.frontierFC);
    map.getSource('replay-leg')        ?.setData(gj.legFC);

    // Route geometry — prefer clipping the decoded-path WKT (available on successful decodes);
    // fall back to assembling from routeSegIds (works for failed decodes where a leg succeeded
    // but the overall decode did not, so wkt is null).
    const currentStep = replaySteps[replayStep];
    const { routeFromSnap, routeToSnap, routeSegIds } = visualState;
    let routeFeats = [];

    // For offset-trim and decode-complete steps, bypass per-leg clipping and show
    // the full decoded path so the animation operates on the entire location.
    const showFullWkt = currentStep?.type === 'offset_applied' ||
      (currentStep?.type === 'decode_complete' && currentStep.outcome?.Success);
    if (showFullWkt) {
      const wktCoords = parseWktLinestring(decodeResultRef.current?.wkt);
      if (wktCoords?.length >= 2)
        routeFeats = [{ type: 'Feature', geometry: { type: 'LineString', coordinates: wktCoords }, properties: {} }];
    }

    if (routeFeats.length === 0 && routeFromSnap && routeToSnap) {
      const wktCoords = parseWktLinestring(decodeResultRef.current?.wkt);
      if (wktCoords?.length >= 2) {
        const seg1 = clipGeomFromPoint(wktCoords,  routeFromSnap[0], routeFromSnap[1]);
        const seg2 = seg1?.length >= 2
          ? clipGeomToPoint(seg1, routeToSnap[0], routeToSnap[1])
          : null;
        if (seg2?.length >= 2) {
          routeFeats = [{ type: 'Feature', geometry: { type: 'LineString', coordinates: seg2 }, properties: {} }];
        }
      }
    }
    if (routeFeats.length === 0 && routeSegIds?.length > 0) {
      // Fallback: assemble from cached segment geometries (handles failed-decode legs).
      const segCache  = getSegGeomCache();
      const segToTile = getSegIdToTile();
      const tileCache = getTileGeomCache();
      routeFeats = routeSegIds.map(id => {
        let f = segCache.get(id);
        if (!f) {
          const m = segToTile.get(id);
          if (m) f = tileCache.get(m.tile_key)?.find(x => x.properties.local_index === m.local_index);
        }
        return f;
      }).filter(Boolean);
    }
    map.getSource('replay-route')?.setData({ type: 'FeatureCollection', features: routeFeats });

    // Reset route line to default green so a prior failure step doesn't leave the colour red.
    if (map.getLayer('replay-route-line')) {
      try {
        map.setPaintProperty('replay-route-line',   'line-color',   '#16a34a');
        map.setPaintProperty('replay-route-line',   'line-width',    6);
        map.setPaintProperty('replay-route-line',   'line-opacity',  1.0);
        map.setPaintProperty('replay-route-casing', 'line-color',   '#0f172a');
        map.setPaintProperty('replay-route-casing', 'line-width',   10);
        map.setPaintProperty('replay-route-casing', 'line-opacity',  1.0);
      } catch (_) {}
    }

    // ── Frontier pulse animation ────────────────────────────────────────────
    if (gj.frontierFC.features.length > 0 && map.getLayer('replay-frontier-circle')) {
      const t0 = performance.now();
      const pulse = (now) => {
        if (!map.getLayer('replay-frontier-circle')) return;
        const phase = ((now - t0) / 600) * Math.PI * 2;
        try {
          map.setPaintProperty('replay-frontier-circle', 'circle-opacity', 0.6 + 0.4 * Math.sin(phase));
          map.setPaintProperty('replay-frontier-circle', 'circle-radius',  5   + 2   * Math.sin(phase));
        } catch (_) { return; }
        frontierPulseRef.current = requestAnimationFrame(pulse);
      };
      frontierPulseRef.current = requestAnimationFrame(pulse);
    }

    // ── Auto-pan ────────────────────────────────────────────────────────────
    if (currentStep?.type === 'search_started') {
      map.flyTo({
        center:   [currentStep.coord[0], currentStep.coord[1]],
        zoom:     Math.max(map.getZoom(), 16),
        duration: 400,
      });
    }

    if (currentStep?.type === 'candidates_ranked') {
      // Collect the LRP coord from the preceding search_started for this LRP.
      const pts = [];
      for (let i = replayStep - 1; i >= 0; i--) {
        const s = replaySteps[i];
        if (s.type === 'search_started' && s.lrp_idx === currentStep.lrp_idx) {
          pts.push(s.coord); break;
        }
      }
      for (const c of currentStep.accepted ?? []) {
        if (c.projection?.point) pts.push(c.projection.point);
      }
      for (const r of currentStep.rejected ?? []) {
        if (r.point) pts.push(r.point);
      }
      if (pts.length > 0) {
        const lons = pts.map(p => p[0]), lats = pts.map(p => p[1]);
        const w = Math.min(...lons), e = Math.max(...lons);
        const s = Math.min(...lats), n = Math.max(...lats);
        if (w === e && s === n) {
          map.flyTo({ center: [w, s], zoom: Math.max(map.getZoom(), 16), duration: 300 });
        } else {
          map.fitBounds([[w, s], [e, n]], { padding: 120, maxZoom: 17, duration: 400 });
        }
      }
    }

    if (currentStep?.type === 'route_search_started') {
      const from = currentStep.from.projection.point;
      const to   = currentStep.to.projection.point;
      map.fitBounds(
        [[Math.min(from[0], to[0]), Math.min(from[1], to[1])],
         [Math.max(from[0], to[0]), Math.max(from[1], to[1])]],
        { padding: 120, maxZoom: 17, duration: 400 },
      );
    }

    // When a leg route is found: pan to full route extent, then pulse the line for 3 s.
    if (currentStep?.type === 'route_found') {
      if (routeFeats.length > 0) {
        let minLon = Infinity, minLat = Infinity, maxLon = -Infinity, maxLat = -Infinity;
        for (const feat of routeFeats) {
          const coords = feat.geometry.type === 'LineString'      ? feat.geometry.coordinates
                       : feat.geometry.type === 'MultiLineString' ? feat.geometry.coordinates.flat()
                       : [];
          for (const [lon, lat] of coords) {
            if (lon < minLon) minLon = lon; if (lon > maxLon) maxLon = lon;
            if (lat < minLat) minLat = lat; if (lat > maxLat) maxLat = lat;
          }
        }
        if (isFinite(minLon)) {
          map.fitBounds([[minLon, minLat], [maxLon, maxLat]], { padding: 80, maxZoom: 17, duration: 600 });
        }
      }
      if (routePulseRef.current) cancelAnimationFrame(routePulseRef.current);
      const rt0 = performance.now();
      const ROUTE_PULSE_MS = 3000;
      const animRoute = (now) => {
        if (!map.getLayer('replay-route-line')) return;
        const elapsed = now - rt0;
        const done    = elapsed >= ROUTE_PULSE_MS;
        const phase   = (elapsed / 500) * Math.PI;
        const swell   = Math.abs(Math.sin(phase));
        try {
          map.setPaintProperty('replay-route-line',   'line-width',   done ? 6  : 5  + 4 * swell);
          map.setPaintProperty('replay-route-line',   'line-opacity', done ? 1.0 : 0.7 + 0.3 * swell);
          map.setPaintProperty('replay-route-casing', 'line-width',   done ? 10 : 9  + 4 * swell);
          map.setPaintProperty('replay-route-casing', 'line-opacity', done ? 1.0 : 0.5 + 0.3 * swell);
        } catch (_) { return; }
        if (!done) routePulseRef.current = requestAnimationFrame(animRoute);
        else routePulseRef.current = null;
      };
      routePulseRef.current = requestAnimationFrame(animRoute);
    }

    // offset_applied: fly to the trimmed endpoint so the user sees where the trim was applied.
    if (currentStep?.type === 'offset_applied' && routeFeats.length > 0) {
      const allCoords = routeFeats.flatMap(f =>
        f.geometry.type === 'LineString' ? f.geometry.coordinates : f.geometry.coordinates.flat()
      );
      if (allCoords.length >= 2) {
        const trimCoord = currentStep.is_positive ? allCoords[0] : allCoords[allCoords.length - 1];
        map.flyTo({ center: trimCoord, zoom: Math.max(map.getZoom(), 15), duration: 500 });
      }
    }

    // decode_complete (Success): zoom to full extent and pulse the whole location.
    if (currentStep?.type === 'decode_complete' && currentStep.outcome?.Success && routeFeats.length > 0) {
      const allCoords = routeFeats.flatMap(f =>
        f.geometry.type === 'LineString' ? f.geometry.coordinates : f.geometry.coordinates.flat()
      );
      if (allCoords.length >= 2) {
        let mnLon = Infinity, mnLat = Infinity, mxLon = -Infinity, mxLat = -Infinity;
        for (const [lo, la] of allCoords) {
          if (lo < mnLon) mnLon = lo; if (lo > mxLon) mxLon = lo;
          if (la < mnLat) mnLat = la; if (la > mxLat) mxLat = la;
        }
        if (isFinite(mnLon))
          map.fitBounds([[mnLon, mnLat], [mxLon, mxLat]], { padding: 80, maxZoom: 17, duration: 600 });
      }
      if (routePulseRef.current) cancelAnimationFrame(routePulseRef.current);
      const dp0 = performance.now();
      const ROUTE_PULSE_MS = 1500;
      const animDone = (now) => {
        if (!map.getLayer('replay-route-line')) return;
        const elapsed = now - dp0;
        const done    = elapsed >= ROUTE_PULSE_MS;
        const swell   = Math.abs(Math.sin((elapsed / 500) * Math.PI));
        try {
          map.setPaintProperty('replay-route-line',   'line-width',   done ? 6  : 5  + 4 * swell);
          map.setPaintProperty('replay-route-line',   'line-opacity', done ? 1.0 : 0.7 + 0.3 * swell);
          map.setPaintProperty('replay-route-casing', 'line-width',   done ? 10 : 9  + 4 * swell);
          map.setPaintProperty('replay-route-casing', 'line-opacity', done ? 1.0 : 0.5 + 0.3 * swell);
        } catch (_) { return; }
        if (!done) routePulseRef.current = requestAnimationFrame(animDone);
        else routePulseRef.current = null;
      };
      routePulseRef.current = requestAnimationFrame(animDone);
    }

    // decode_complete (Failure): pan to the troublesome area and pulse red.
    if (currentStep?.type === 'decode_complete' && !currentStep.outcome?.Success) {
      // Find the last routing leg's snap coordinates by scanning backward.
      let failBounds = null;
      for (let i = replayStep - 1; i >= 0; i--) {
        const s = replaySteps[i];
        if (s.type === 'route_search_started') {
          const from = s.from?.projection?.point;
          const to   = s.to?.projection?.point;
          if (from && to) {
            failBounds = [[
              Math.min(from[0], to[0]) - 0.001,
              Math.min(from[1], to[1]) - 0.001,
            ], [
              Math.max(from[0], to[0]) + 0.001,
              Math.max(from[1], to[1]) + 0.001,
            ]];
            break;
          }
        }
      }
      if (routeFeats.length > 0) {
        const allCoords = routeFeats.flatMap(f =>
          f.geometry.type === 'LineString' ? f.geometry.coordinates : f.geometry.coordinates.flat()
        );
        if (allCoords.length >= 2) {
          let mnLon = Infinity, mnLat = Infinity, mxLon = -Infinity, mxLat = -Infinity;
          for (const [lo, la] of allCoords) {
            if (lo < mnLon) mnLon = lo; if (lo > mxLon) mxLon = lo;
            if (la < mnLat) mnLat = la; if (la > mxLat) mxLat = la;
          }
          if (isFinite(mnLon))
            map.fitBounds([[mnLon, mnLat], [mxLon, mxLat]], { padding: 80, maxZoom: 17, duration: 600 });
        }
      } else if (failBounds) {
        map.fitBounds(failBounds, { padding: 100, maxZoom: 17, duration: 600 });
      }
      // Pulse red — only meaningful when there is something to pulse.
      if (routeFeats.length > 0 || failBounds) {
        try {
          map.setPaintProperty('replay-route-line',   'line-color', '#ef4444');
          map.setPaintProperty('replay-route-casing', 'line-color', '#7f1d1d');
        } catch (_) {}
        if (routePulseRef.current) cancelAnimationFrame(routePulseRef.current);
        const fp0 = performance.now();
        const FAIL_PULSE_MS = 3000;
        const animFail = (now) => {
          if (!map.getLayer('replay-route-line')) return;
          const elapsed = now - fp0;
          const done    = elapsed >= FAIL_PULSE_MS;
          const swell   = Math.abs(Math.sin((elapsed / 500) * Math.PI));
          try {
            map.setPaintProperty('replay-route-line',   'line-width',   done ? 6  : 5  + 4 * swell);
            map.setPaintProperty('replay-route-line',   'line-opacity', done ? 1.0 : 0.7 + 0.3 * swell);
            map.setPaintProperty('replay-route-casing', 'line-width',   done ? 10 : 9  + 4 * swell);
            map.setPaintProperty('replay-route-casing', 'line-opacity', done ? 1.0 : 0.5 + 0.3 * swell);
          } catch (_) { return; }
          if (!done) routePulseRef.current = requestAnimationFrame(animFail);
          else routePulseRef.current = null;
        };
        routePulseRef.current = requestAnimationFrame(animFail);
      }
    }

    // Follow each A* node: instant jump so playback stays in sync.
    // Zoom 17 ≈ 700 m viewport width on a 1200 px screen — a typical road
    // segment (100–300 m) fills roughly half the map.
    if (currentStep?.type === 'astar_batch') {
      const last = currentStep.nodes[currentStep.nodes.length - 1];
      map.jumpTo({ center: [last.lon, last.lat], zoom: 17 });

      // Sonar-ping: expanding cyan ring that fades out over 2 s.
      // During rapid auto-play the ring stays bright (reset every 30 ms);
      // it fades only when stepping pauses.
      const flashSrc = map.getSource('replay-flash');
      if (flashSrc && map.getLayer('replay-flash-ring')) {
        flashSrc.setData({
          type: 'FeatureCollection',
          features: [{ type: 'Feature', geometry: { type: 'Point', coordinates: [last.lon, last.lat] }, properties: {} }],
        });
        if (flashAnimRef.current) cancelAnimationFrame(flashAnimRef.current);
        const t0 = performance.now();
        const FLASH_MS = 2000;
        const animFlash = (now) => {
          if (!map.getLayer('replay-flash-ring')) return;
          const p = Math.min(1, (now - t0) / FLASH_MS);
          try {
            map.setPaintProperty('replay-flash-ring', 'circle-stroke-opacity', 1 - p);
            map.setPaintProperty('replay-flash-ring', 'circle-radius', 20 + 18 * p);
          } catch (_) { return; }
          if (p < 1) {
            flashAnimRef.current = requestAnimationFrame(animFlash);
          } else {
            flashSrc.setData({ type: 'FeatureCollection', features: [] });
          }
        };
        flashAnimRef.current = requestAnimationFrame(animFlash);
      }
    }
  }, [showReplay, replayStep, replaySteps, replayStats]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Segment layer visibility toggle ──────────────────────────────────────────

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;
    const vis = showSegmentLayer ? 'visible' : 'none';
    for (let frc = 0; frc < 8; frc++) {
      if (map.getLayer(`olr-frc${frc}`)) map.setLayoutProperty(`olr-frc${frc}`, 'visibility', vis);
    }
    if (map.getLayer('olr-highlight'))     map.setLayoutProperty('olr-highlight',     'visibility', vis);
    if (map.getLayer('olr-nodes-circle')) map.setLayoutProperty('olr-nodes-circle', 'visibility', vis);
    // Turning the layer on doesn't imply a pan/zoom, so the moveend/zoomend
    // listeners that normally trigger tile loading never fire — load explicitly.
    if (showSegmentLayer) loadVisibleTiles(map);
  }, [showSegmentLayer]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Decode result → map layers + camera ─────────────────────────────────────

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    const pathSource        = map.getSource('decoded-path');
    const lrpSource         = map.getSource('lrp-markers');
    const snapSource        = map.getSource('lrp-snap');
    const displSource       = map.getSource('lrp-displacement');
    const uncertaintySource = map.getSource('offset-uncertainty');
    const palSource         = map.getSource('pal-point');
    const palPulseSource    = map.getSource('pal-pulse');
    const palUncertaintySource = map.getSource('pal-uncertainty');

    // Cancel any in-progress PAL pulse from a previous decode.
    if (palPulseRef.current) { cancelAnimationFrame(palPulseRef.current); palPulseRef.current = null; }
    if (palPulseSource && map.getLayer('pal-pulse-ring')) {
      try { map.setPaintProperty('pal-pulse-ring', 'circle-stroke-opacity', 0); } catch (_) {}
      palPulseSource.setData({ type: 'FeatureCollection', features: [] });
    }

    const emptyFC = { type: 'FeatureCollection', features: [] };
    // Encode mode has its own waypoint/route rendering — the last actual
    // decode's markers/path/etc. would otherwise keep showing underneath it,
    // since switching mode back to 'encode' doesn't touch `decodeResult` at
    // all (nothing re-populates or clears it), so this effect would never
    // re-run without `mode` in its own right.
    if (!decodeResult || mode !== 'decode') {
      pathSource?.setData(emptyFC);
      lrpSource?.setData(emptyFC);
      snapSource?.setData(emptyFC);
      displSource?.setData(emptyFC);
      uncertaintySource?.setData(emptyFC);
      palSource?.setData(emptyFC);
      palUncertaintySource?.setData(emptyFC);
      closeAllPopups();
      return;
    }

    // ── LRP markers (success and failure) ────────────────────────────────────
    const lrps = decodeResult.lrps ?? [];
    lrpSource?.setData({
      type: 'FeatureCollection',
      features: lrps.map((lrp, idx) => ({
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [lrp.lon, lrp.lat] },
        properties: {
          index: idx, total: lrps.length, lat: lrp.lat, lon: lrp.lon,
          frc: lrp.frc, fow: lrp.fow,
          lfrcnp: lrp.lfrcnp ?? null,
          bearing_lb: lrp.bearing_lb, bearing_ub: lrp.bearing_ub,
          snap_lon: lrp.snap_lon ?? null,
          snap_lat: lrp.snap_lat ?? null,
          snap_is_endpoint: lrp.snap_is_endpoint ?? null,
          snap_distance_m: lrp.snap_distance_m ?? null,
        },
      })),
    });

    // ── Snap markers and displacement lines ───────────────────────────────────
    const snapFeatures = lrps
      .filter(lrp => lrp.snap_lon != null)
      .map((lrp, idx) => ({
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [lrp.snap_lon, lrp.snap_lat] },
        properties: {
          index: idx,
          is_endpoint: lrp.snap_is_endpoint ?? false,
          bearing: compassBearing(lrp.lon, lrp.lat, lrp.snap_lon, lrp.snap_lat),
        },
      }));
    snapSource?.setData({ type: 'FeatureCollection', features: snapFeatures });

    const displFeatures = lrps
      .filter(lrp => lrp.snap_lon != null)
      .map((lrp, idx) => ({
        type: 'Feature',
        geometry: { type: 'LineString', coordinates: [[lrp.lon, lrp.lat], [lrp.snap_lon, lrp.snap_lat]] },
        properties: { index: idx },
      }));
    displSource?.setData({ type: 'FeatureCollection', features: displFeatures });

    // ── Decoded path — use WKT for correctly offset-trimmed display ───────────
    // Per-segment geometries span full segments and ignore arc-offset trim;
    // the WKT from path_to_wkt already applies first_lrp_arc + pos_offset at
    // the head and last_lrp_arc - neg_offset at the tail.
    // PAL has no "path" of its own to show — it's just the reference line
    // the point sits on — so skip it there and let the LRP markers + point +
    // uncertainty stub speak for themselves without a distracting full line.
    const isPalResult = decodeResult.location_type === 'PointAlongLine';
    const wktCoords = parseWktLinestring(decodeResult.wkt);
    const pathFeatures = (decodeResult.ok && !isPalResult && wktCoords?.length >= 2)
      ? [{ type: 'Feature', geometry: { type: 'LineString', coordinates: wktCoords }, properties: {} }]
      : [];
    pathSource?.setData({ type: 'FeatureCollection', features: pathFeatures });

    // ── Offset uncertainty bands ──────────────────────────────────────────────
    // Shown only when the offset is a v3 bucket interval (lb < ub).
    const uncertaintyFeatures = [];
    for (const [wkt, label] of [
      [decodeResult.pos_uncertainty_wkt, 'pos'],
      [decodeResult.neg_uncertainty_wkt, 'neg'],
    ]) {
      if (wkt) {
        const coords = parseWktLinestring(wkt);
        if (coords?.length >= 2) {
          uncertaintyFeatures.push({
            type: 'Feature',
            geometry: { type: 'LineString', coordinates: coords },
            properties: { label },
          });
        }
      }
    }
    uncertaintySource?.setData({ type: 'FeatureCollection', features: uncertaintyFeatures });

    // ── PointAlongLine result point ───────────────────────────────────────────
    if (decodeResult.ok && decodeResult.location_type === 'PointAlongLine' &&
        decodeResult.point_lon != null && decodeResult.point_lat != null) {
      const palPointFC = {
        type: 'FeatureCollection',
        features: [{
          type: 'Feature',
          geometry: { type: 'Point', coordinates: [decodeResult.point_lon, decodeResult.point_lat] },
          properties: {
            orientation: decodeResult.orientation,
            side_of_road: decodeResult.side_of_road,
          },
        }],
      };
      palSource?.setData(palPointFC);
      palPulseSource?.setData(palPointFC);

      // POFF uncertainty — the v3-encoded offset is a quantization bucket
      // [lb, ub], not an exact distance; the point marker sits at the
      // bucket's center (`point_lon`/`point_lat`, computed that way engine-
      // side), and this dashed segment spans the bucket itself so the
      // uncertainty is visible rather than implied by a single dot.
      if (decodeResult.point_lon_lb != null && decodeResult.point_lon_ub != null) {
        palUncertaintySource?.setData({
          type: 'FeatureCollection',
          features: [{
            type: 'Feature',
            geometry: {
              type: 'LineString',
              coordinates: [
                [decodeResult.point_lon_lb, decodeResult.point_lat_lb],
                [decodeResult.point_lon_ub, decodeResult.point_lat_ub],
              ],
            },
            properties: {},
          }],
        });
      } else {
        palUncertaintySource?.setData(emptyFC);
      }

      // Two sonar-ping pulses over 2 s, then stop.
      if (palPulseSource && map.getLayer('pal-pulse-ring')) {
        const TOTAL_MS = 2000, CYCLE_MS = 1000;
        const t0 = performance.now();
        const animPalPulse = (now) => {
          if (!map.getLayer('pal-pulse-ring')) { palPulseRef.current = null; return; }
          const elapsed = now - t0;
          if (elapsed >= TOTAL_MS) {
            try { map.setPaintProperty('pal-pulse-ring', 'circle-stroke-opacity', 0); } catch (_) {}
            palPulseSource.setData({ type: 'FeatureCollection', features: [] });
            palPulseRef.current = null;
            return;
          }
          const p = (elapsed % CYCLE_MS) / CYCLE_MS;
          try {
            map.setPaintProperty('pal-pulse-ring', 'circle-radius',         8 + 24 * p);
            map.setPaintProperty('pal-pulse-ring', 'circle-stroke-opacity', 0.85 * (1 - p));
          } catch (_) { palPulseRef.current = null; return; }
          palPulseRef.current = requestAnimationFrame(animPalPulse);
        };
        palPulseRef.current = requestAnimationFrame(animPalPulse);
      }
    } else {
      palSource?.setData(emptyFC);
      palUncertaintySource?.setData(emptyFC);
    }

    // ── Fit camera — always include all LRP positions AND the decoded path ──────
    const isPalDecode = decodeResult.ok && decodeResult.location_type === 'PointAlongLine' &&
                        decodeResult.point_lon != null && decodeResult.point_lat != null;
    const lrpCoords = lrps.map(l => [l.lon, l.lat]);
    const fitCoords = [
      ...lrpCoords,
      ...(wktCoords ?? []),
    ];

    // Cancel any in-progress post-decode fade
    if (decodePulseRef.current) { cancelAnimationFrame(decodePulseRef.current); decodePulseRef.current = null; }

    if (isPalDecode) {
      // Center on the decoded point so it's immediately prominent.
      requestAnimationFrame(() => {
        map.flyTo({ center: [decodeResult.point_lon, decodeResult.point_lat], zoom: Math.max(map.getZoom(), 16), duration: 600 });
      });
    } else if (fitCoords.length > 0) {
      const lngs = fitCoords.map(c => c[0]);
      const lats = fitCoords.map(c => c[1]);
      const bounds = [[Math.min(...lngs), Math.min(...lats)], [Math.max(...lngs), Math.max(...lats)]];
      requestAnimationFrame(() => {
        map.fitBounds(bounds, { padding: 80, duration: 600, maxZoom: 17 });
        // Runs the pulse in parallel with the camera move rather than
        // waiting on `moveend` — if the camera's already roughly where it
        // needs to be (e.g. decoding a reference for a location you just
        // finished drawing waypoints on), fitBounds is a no-op and
        // `moveend` may never fire, silently skipping the animation.
        if (decodeResult?.ok) {
          if (!map.getLayer('decoded-path-line')) return;
          const GREEN = [22, 163, 74];
          const CYAN  = [0, 212, 255];
          const lerpRgb = (a, b, t) =>
            `rgb(${Math.round(a[0]+(b[0]-a[0])*t)},${Math.round(a[1]+(b[1]-a[1])*t)},${Math.round(a[2]+(b[2]-a[2])*t)})`;
          const PULSE_MS = 1500, FADE_MS = 1500;
          const t0 = performance.now();
          const anim = (now) => {
            if (!map.getLayer('decoded-path-line')) return;
            const elapsed = now - t0;
            try {
              if (elapsed < PULSE_MS) {
                const swell = Math.abs(Math.sin((elapsed / 500) * Math.PI));
                map.setPaintProperty('decoded-path-line', 'line-color',   '#16a34a');
                map.setPaintProperty('decoded-path-line', 'line-width',   5 + 4 * swell);
                map.setPaintProperty('decoded-path-line', 'line-opacity', 0.7 + 0.3 * swell);
                decodePulseRef.current = requestAnimationFrame(anim);
              } else if (elapsed < PULSE_MS + FADE_MS) {
                const t = (elapsed - PULSE_MS) / FADE_MS;
                map.setPaintProperty('decoded-path-line', 'line-color',   lerpRgb(GREEN, CYAN, t));
                map.setPaintProperty('decoded-path-line', 'line-width',   9 - 4 * t);
                map.setPaintProperty('decoded-path-line', 'line-opacity', 0.9);
                decodePulseRef.current = requestAnimationFrame(anim);
              } else {
                map.setPaintProperty('decoded-path-line', 'line-color',   '#00d4ff');
                map.setPaintProperty('decoded-path-line', 'line-width',   5);
                map.setPaintProperty('decoded-path-line', 'line-opacity', 0.9);
                decodePulseRef.current = null;
              }
            } catch (_) { decodePulseRef.current = null; }
          };
          decodePulseRef.current = requestAnimationFrame(anim);
        }
      });
    }
  }, [decodeResult, mode]);

  // ── Measurement tool ──────────────────────────────────────────────────────────

  function toggleMeasure() {
    if (measuringRef.current) {
      measuringRef.current = false;
      measurePtsRef.current = [];
      setMeasuring(false);
      setMeasurePts([]);
      setMeasureCursor(null);
    } else {
      measuringRef.current = true;
      measurePtsRef.current = [];
      setMeasuring(true);
      setMeasurePts([]);
    }
  }

  // Activate/deactivate measure mode: cursor, mousemove, dblclick.
  useEffect(() => {
    measuringRef.current = measuring;
    const map = mapRef.current;
    if (!map) return;
    if (!measuring) {
      map.getCanvas().style.cursor = '';
      return;
    }
    map.getCanvas().style.cursor = 'crosshair';
    map.doubleClickZoom.disable();

    const onMove = (e) => setMeasureCursor([e.lngLat.lng, e.lngLat.lat]);
    const onDblClick = () => {
      // The second click of the dblclick already added a point via onMapClick;
      // remove that spurious duplicate and finish.
      const trimmed = measurePtsRef.current.slice(0, -1);
      measurePtsRef.current = trimmed;
      setMeasurePts([...trimmed]);
      measuringRef.current = false;
      setMeasuring(false);
      setMeasureCursor(null);
    };

    map.on('mousemove', onMove);
    map.on('dblclick', onDblClick);
    return () => {
      map.off('mousemove', onMove);
      map.off('dblclick', onDblClick);
      map.doubleClickZoom.enable();
      if (!measuringRef.current) map.getCanvas().style.cursor = '';
    };
  }, [measuring]); // eslint-disable-line react-hooks/exhaustive-deps

  // Escape cancels measurement.
  useEffect(() => {
    const onKey = (e) => {
      if (e.key === 'Escape') {
        if (measuringRef.current) {
          measuringRef.current = false;
          measurePtsRef.current = [];
          setMeasuring(false);
          setMeasurePts([]);
          setMeasureCursor(null);
        } else if (bearingActiveRef.current) {
          bearingActiveRef.current = false;
          bearingPtsRef.current = [];
          setBearingActive(false);
          setBearingPts([]);
        }
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, []);

  // Sync measure GeoJSON sources whenever points or cursor change.
  useEffect(() => {
    const map = mapRef.current;
    if (!map || !map.getSource('measure-line')) return;
    const pts = measureCursor ? [...measurePts, measureCursor] : measurePts;
    const lineData = pts.length >= 2
      ? { type: 'FeatureCollection', features: [{ type: 'Feature', geometry: { type: 'LineString', coordinates: pts }, properties: {} }] }
      : { type: 'FeatureCollection', features: [] };
    const pointsData = {
      type: 'FeatureCollection',
      features: measurePts.map(pt => ({ type: 'Feature', geometry: { type: 'Point', coordinates: pt }, properties: {} })),
    };
    map.getSource('measure-line').setData(lineData);
    map.getSource('measure-points').setData(pointsData);
  }, [measurePts, measureCursor]);

  // ── Bearing tool ──────────────────────────────────────────────────────────────

  function toggleBearing() {
    if (bearingActiveRef.current) {
      bearingActiveRef.current = false;
      bearingPtsRef.current = [];
      setBearingActive(false);
      setBearingPts([]);
    } else {
      bearingActiveRef.current = true;
      bearingPtsRef.current = [];
      setBearingActive(true);
      setBearingPts([]);
    }
  }

  useEffect(() => {
    bearingActiveRef.current = bearingActive;
    const map = mapRef.current;
    if (!map) return;
    if (!bearingActive) {
      if (!measuringRef.current) map.getCanvas().style.cursor = '';
      return;
    }
    map.getCanvas().style.cursor = 'crosshair';
    return () => {
      if (!bearingActiveRef.current && !measuringRef.current) map.getCanvas().style.cursor = '';
    };
  }, [bearingActive]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    encodeModeRef.current = mode === 'encode';
    const map = mapRef.current;
    if (!map) return;
    if (mode !== 'encode') {
      if (!measuringRef.current && !bearingActiveRef.current && !coordCaptureActiveRef.current) map.getCanvas().style.cursor = '';
      return;
    }
    map.getCanvas().style.cursor = 'crosshair';
    return () => {
      if (!encodeModeRef.current && !measuringRef.current && !bearingActiveRef.current && !coordCaptureActiveRef.current) map.getCanvas().style.cursor = '';
    };
  }, [mode]); // eslint-disable-line react-hooks/exhaustive-deps

  function doPermalink() {
    const url = `${window.location.origin}${window.location.pathname}#q=${encodeURIComponent(openlrString)}`;
    navigator.clipboard.writeText(url).catch(() => {});
    setPermalinkCopied(true);
    setTimeout(() => setPermalinkCopied(false), 1500);
  }

  function doZoomGo() {
    const parsed = parseLatLon(zoomInput);
    if (!parsed) { setZoomError(true); return; }
    setZoomError(false);
    const id = `lp-${parsed.lat.toFixed(6)}-${parsed.lon.toFixed(6)}-${performance.now().toFixed(0)}`;
    setLocPins(prev => [...prev, { id, lat: parsed.lat, lon: parsed.lon }]);
    mapRef.current?.flyTo({ center: [parsed.lon, parsed.lat], zoom: 16, duration: 800 });
  }

  function toggleCoordCapture() {
    const nowActive = !coordCaptureActive;
    coordCaptureActiveRef.current = nowActive;
    setCoordCaptureActive(nowActive);
    const canvas = mapRef.current?.getCanvas();
    if (canvas) canvas.style.cursor = nowActive ? 'crosshair' : '';
    if (!nowActive) {
      setCursorCoord(null);
      cursorCoordRef.current = null;
      if (capturePopupRef.current) { capturePopupRef.current.remove(); capturePopupRef.current = null; }
    }
  }

  function commitCoordCapture() {
    const coord = cursorCoordRef.current;
    if (!coord) return;
    const [lon, lat] = coord;
    const text = `${lat.toFixed(6)}, ${lon.toFixed(6)}`;

    // Copy immediately
    navigator.clipboard.writeText(text).catch(() => {});
    setCopiedText(text);
    setCoordCopied(true);
    setTimeout(() => { setCoordCopied(false); setCopiedText(''); }, 1500);

    // Deactivate capture mode
    coordCaptureActiveRef.current = false;
    setCoordCaptureActive(false);
    setCursorCoord(null);
    cursorCoordRef.current = null;
    const canvas = mapRef.current?.getCanvas();
    if (canvas) canvas.style.cursor = '';

    // Show popup at the captured location offering "Add pin"
    const map = mapRef.current;
    if (!map) return;
    if (capturePopupRef.current) { capturePopupRef.current.remove(); capturePopupRef.current = null; }

    const content = document.createElement('div');
    content.className = 'loc-pin-popup';
    content.innerHTML = `<div class="loc-pin-coord">✓ Copied: ${text}</div>
      <div class="loc-pin-btns"><button class="loc-pin-dismiss capture-addpin-btn">Add pin</button></div>`;

    const popup = new maplibregl.Popup({ closeButton: true, offset: 0, className: 'loc-pin-popup-wrap' })
      .setLngLat([lon, lat])
      .setDOMContent(content)
      .addTo(map);

    capturePopupRef.current = popup;
    popup.on('close', () => { capturePopupRef.current = null; });

    content.querySelector('.capture-addpin-btn').addEventListener('click', () => {
      const id = `lp-${lat.toFixed(6)}-${lon.toFixed(6)}-${performance.now().toFixed(0)}`;
      setLocPins(prev => [...prev, { id, lat, lon }]);
      popup.remove();
    });
  }

  useEffect(() => {
    if (!coordCaptureActive) return;
    function onKeyDown(e) {
      if (e.key === 'Enter') { e.preventDefault(); commitCoordCapture(); }
      else if (e.key === 'Escape') { toggleCoordCapture(); }
    }
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, [coordCaptureActive]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Location pin markers ─────────────────────────────────────────────────────
  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    // Remove markers for pins that were dismissed
    const currentIds = new Set(locPins.map(p => p.id));
    Object.keys(locPinMarkersRef.current).forEach(id => {
      if (!currentIds.has(id)) {
        locPinMarkersRef.current[id].marker.remove();
        delete locPinMarkersRef.current[id];
      }
    });

    // Add markers for new pins
    locPins.forEach(pin => {
      if (locPinMarkersRef.current[pin.id]) return;

      const el = document.createElement('div');
      el.className = 'loc-pin-marker';
      el.textContent = '📍';

      const content = document.createElement('div');
      content.className = 'loc-pin-popup';
      content.innerHTML = `<div class="loc-pin-coord">${pin.lat.toFixed(6)}, ${pin.lon.toFixed(6)}</div>
        <div class="loc-pin-btns">
          <button class="loc-pin-dismiss">Dismiss</button>
          <button class="loc-pin-dismiss-all">Dismiss all</button>
        </div>`;
      content.querySelector('.loc-pin-dismiss').addEventListener('click', () =>
        setLocPins(prev => prev.filter(p => p.id !== pin.id)));
      content.querySelector('.loc-pin-dismiss-all').addEventListener('click', () =>
        setLocPins([]));

      const popup = new maplibregl.Popup({ closeButton: true, offset: 28, className: 'loc-pin-popup-wrap' })
        .setDOMContent(content);

      const marker = new maplibregl.Marker({ element: el, anchor: 'bottom' })
        .setLngLat([pin.lon, pin.lat])
        .setPopup(popup)
        .addTo(map);

      locPinMarkersRef.current[pin.id] = { marker, popup };
    });
  }, [locPins]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Encode mode: live route line ─────────────────────────────────────────────
  // Gated on mode — `liveRoute` deliberately survives a mode switch (so
  // waypoints/preview are still there if you switch back to Encode), which
  // means this effect would otherwise never re-run to clear itself when you
  // leave encode mode, leaving the green route + casing visible underneath
  // whatever decode mode draws next.
  useEffect(() => {
    const map = mapRef.current;
    if (!map || !map.getSource('encode-route')) return;
    const coords = mode === 'encode' ? (liveRoute?.geometry ?? []) : [];
    map.getSource('encode-route').setData(
      coords.length >= 2
        ? { type: 'Feature', geometry: { type: 'LineString', coordinates: coords }, properties: {} }
        : { type: 'FeatureCollection', features: [] }
    );
  }, [liveRoute, mode]);

  // ── Encode mode: waypoint → snapped-node offset stubs ────────────────────────
  // A waypoint is not an LRP — it's just where the encoder starts looking for
  // one. Visualize the gap between the click and the actual anchor node, so
  // it's clear the route wasn't drawn "wrong", it just doesn't start exactly
  // where you clicked. Same mode-gating reasoning as the route line above.
  useEffect(() => {
    const map = mapRef.current;
    if (!map || !map.getSource('encode-offset-stubs')) return;
    const features = [];
    if (mode === 'encode') {
      const snapped = liveRoute?.snapped ?? [];
      waypoints.forEach((wp, i) => {
        const snap = snapped[i];
        if (!snap) return;
        const dLon = (snap[0] - wp.lon) * Math.cos(wp.lat * Math.PI / 180);
        const dLat = snap[1] - wp.lat;
        const distM = Math.sqrt(dLon * dLon + dLat * dLat) * 111_000;
        if (distM < 2) return; // negligible — don't clutter the map
        const category = (i === 0 || i === waypoints.length - 1) ? 'boundary' : 'via';
        features.push({
          type: 'Feature',
          geometry: { type: 'LineString', coordinates: [[wp.lon, wp.lat], snap] },
          properties: { category },
        });
      });
    }
    map.getSource('encode-offset-stubs').setData({ type: 'FeatureCollection', features });
  }, [waypoints, liveRoute, mode]);

  // ── Encode mode: waypoint markers ────────────────────────────────────────────
  // Rebuilt in full on every change — waypoint lists are short (a handful of
  // points), so this is simpler and cheap enough compared to id-based diffing.
  //
  // Markers are NOT draggable via MapLibre's native `Marker.draggable` —
  // that's built around left/primary-button drag, and this app dedicates
  // the right button entirely to waypoint editing (see onEncodeMouseDown).
  // Moving a marker is instead a right-mousedown on the marker's own DOM
  // element (which starts the same ghost-drag + popup flow insert/add use,
  // via startEncodeDragRef — the map-level mousedown handler lives in a
  // different effect, so this ref is how the two meet). Plain left-click
  // still removes the waypoint directly.
  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;
    waypointMarkersRef.current.forEach(m => m.remove());
    waypointMarkersRef.current = [];
    if (mode !== 'encode') return;

    waypoints.forEach((wp, i) => {
      const el = document.createElement('div');
      el.className = 'encode-waypoint-marker';
      el.textContent = String(i + 1);
      el.title = 'Right-click to move · click to remove';

      const marker = new maplibregl.Marker({ element: el, draggable: false, anchor: 'center' })
        .setLngLat([wp.lon, wp.lat])
        .addTo(map);

      el.addEventListener('mousedown', (ev) => {
        if (ev.button !== 2) return;
        ev.preventDefault();
        ev.stopPropagation();
        startEncodeDragRef.current?.('move', i, { lngLat: marker.getLngLat() });
      });
      el.addEventListener('click', (ev) => {
        ev.stopPropagation();
        removeWaypoint(i);
      });

      waypointMarkersRef.current.push(marker);
    });
  }, [waypoints, mode]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Render ───────────────────────────────────────────────────────────────────

  return (
    <div className="map-wrap">
      <div ref={mapContainer} className="map-container" />

      {/* Status overlay */}
      {status && <div className="map-status">{status}</div>}

      {/* Segment info panel */}
      {infoProps && infoAnchor && (() => {
        const { style: segStyle, placement: segPl, tipLeft: segTipLeft } = popupPlacement(infoAnchor, 260, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div ref={segPanelRef}
          className="seg-info-panel"
          style={segPos ? { position: 'absolute', left: segPos.left, top: segPos.top, right: 'auto', bottom: 'auto' } : segStyle}>
          <header className="seg-info-header" onMouseDown={segMouseDown}>
            <span>
              Segment{infoProps.stable_id != null ? ` ${infoProps.stable_id}` : ''}
            </span>
            <button
              className="seg-info-close"
              onClick={() => {
                setHighlightedSegment(null);
                setInfoProps(null);
                setInfoAnchor(null);
              }}
            >
              ✕
            </button>
          </header>
          <div className="seg-info-body">
            <table>
              <tbody>
                {[
                  ['FRC',       `${infoProps.frc_name} (${infoProps.frc})`],
                  ['FOW',       `${infoProps.fow_name} (${infoProps.fow})`],
                  ['Direction', infoProps.direction],
                  ['Length',    `${infoProps.length_m} m`],
                  ['Tile',         infoProps.tile],
                  ['Tile Index',   infoProps.local_index],
                  ['Start Node',   infoProps.start_node  ?? '—'],
                  ['End Node',     infoProps.end_node    ?? '—'],
                  ['Segment Key',  infoProps.stable_id   ?? '—'],
                  ['Internal ID',  infoProps.segment_id  != null ? infoProps.segment_id : '— (decode first)'],
                ].map(([k, v]) => (
                  <tr key={k}>
                    <td className="seg-info-key">{k}</td>
                    <td><b>{v}</b></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {decodeResult && !segDiagnosis && (
            <button
              className="seg-diag-btn"
              onClick={() => setSegDiagnosis(diagnoseSegment(
                infoProps.segment_id ?? null,
                infoProps,
                decodeResult,
                decodeResult?.trace?.params?.lfrcnp_tolerance ?? lfrcnpTolerance,
              ))}
            >
              Why didn't the location cover this segment?
            </button>
          )}
          {segDiagnosis && (
            <div className="seg-diag-body">
              <div className="seg-diag-headline">{segDiagnosis.headline}</div>
              {segDiagnosis.bullets.length > 0 && (
                <ul className="seg-diag-list">
                  {segDiagnosis.bullets.map((b, i) => <li key={i}>{b}</li>)}
                </ul>
              )}
              {segDiagnosis.suggestions.length > 0 && (
                <div className="seg-diag-suggestions">
                  <span className="seg-diag-try">Try:</span>
                  <ul className="seg-diag-list">
                    {segDiagnosis.suggestions.map((s, i) => <li key={i}>{s}</li>)}
                  </ul>
                </div>
              )}
              <button className="seg-diag-back" onClick={() => setSegDiagnosis(null)}>
                ↩ Back
              </button>
            </div>
          )}
          {!segPos && <TipSvg placement={segPl} tipLeft={segTipLeft} />}
        </div>
        );
      })()}

      {/* LRP info panel */}
      {lrpInfo && infoAnchor && (() => {
        const { style: lrpStyle, placement: lrpPl, tipLeft: lrpTipLeft } = popupPlacement(infoAnchor, 260, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div ref={lrpPanelRef}
          className="seg-info-panel"
          style={lrpPos ? { position: 'absolute', left: lrpPos.left, top: lrpPos.top, right: 'auto', bottom: 'auto' } : lrpStyle}>
          <header className="seg-info-header" onMouseDown={lrpMouseDown}>
            <span>LRP {lrpInfo.index + 1}</span>
            <button className="seg-info-close" onClick={() => { setLrpInfo(null); setInfoAnchor(null); }}>✕</button>
          </header>
          <div className="seg-info-body">
            <table>
              <tbody>
                {[
                  ['Lat',     lrpInfo.lat.toFixed(6)],
                  ['Lon',     lrpInfo.lon.toFixed(6)],
                  ['FRC',     lrpInfo.frc],
                  ['FOW',     lrpInfo.fow],
                  ['LFRCNP',  lrpInfo.lfrcnp !== null
                    ? (lfrcnpTolerance > 0
                      ? `${lrpInfo.lfrcnp} → ${Math.min(lrpInfo.lfrcnp + lfrcnpTolerance, 7)}`
                      : lrpInfo.lfrcnp)
                    : '— (last LRP)'],
                  ['Bearing', formatBearing(lrpInfo.bearing_lb, lrpInfo.bearing_ub)],
                ].map(([k, v]) => (
                  <tr key={k}><td className="seg-info-key">{k}</td><td><b>{v}</b></td></tr>
                ))}
                <tr>
                  <td colSpan={2} style={{ paddingTop: '4px' }}>
                    <BearingCompass lb={lrpInfo.bearing_lb} ub={lrpInfo.bearing_ub} />
                  </td>
                </tr>
                {lrpInfo.snap_lon != null && <>
                  <tr><td className="seg-info-divider" colSpan={2} /></tr>
                  <tr>
                    <td className="seg-info-key">Snap</td>
                    <td><b>{lrpInfo.snap_is_endpoint ? 'Endpoint' : 'Interior'}</b></td>
                  </tr>
                  <tr>
                    <td className="seg-info-key">Displacement</td>
                    <td><b>{Number(lrpInfo.snap_distance_m).toFixed(1)} m</b></td>
                  </tr>
                  <tr>
                    <td className="seg-info-key">Snap coord</td>
                    <td><b style={{fontSize:'11px'}}>{Number(lrpInfo.snap_lat).toFixed(6)}, {Number(lrpInfo.snap_lon).toFixed(6)}</b></td>
                  </tr>
                </>}
              </tbody>
            </table>
          </div>
          {!lrpPos && <TipSvg placement={lrpPl} tipLeft={lrpTipLeft} />}
        </div>
        );
      })()}

      {/* Node intersection popup */}
      {nodeInfo && (() => {
        const { style: nodeStyle, placement: nodePl, tipLeft: nodeTipLeft } = popupPlacement(nodeAnchor, 260, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div
          className="seg-info-panel"
          style={nodeStyle}>
          <header className="seg-info-header">
            <span>Node {nodeInfo.local_index}</span>
            <button className="seg-info-close" onClick={() => { setNodeInfo(null); setNodeAnchor(null); }}>✕</button>
          </header>
          <div className="seg-info-body">
            <table>
              <tbody>
                {[
                  ['Lat',         Number(nodeInfo.lat).toFixed(6)],
                  ['Lon',         Number(nodeInfo.lon).toFixed(6)],
                  ['Tile',        nodeInfo.tile],
                  ['Tile Index',  nodeInfo.local_index],
                  ['ID',          nodeInfo.stable_id ?? '—'],
                  ['Internal ID', nodeInfo.node_id != null ? nodeInfo.node_id : '— (decode first)'],
                ].map(([k, v]) => (
                  <tr key={k}><td className="seg-info-key">{k}</td><td><b>{v}</b></td></tr>
                ))}
              </tbody>
            </table>
          </div>
          <TipSvg placement={nodePl} tipLeft={nodeTipLeft} />
        </div>
        );
      })()}

      {/* Candidate info popup */}
      {candidatePopup && candAnchor && (() => {
        const { style: candStyle, placement: candPl, tipLeft: candTipLeft } = popupPlacement(candAnchor, 320, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div ref={candPanelRef}
          className="seg-info-panel cand-panel"
          style={candPos ? { position: 'absolute', left: candPos.left, top: candPos.top, right: 'auto', bottom: 'auto' } : candStyle}>
          <header className="seg-info-header" onMouseDown={candMouseDown}>
            <span>
              LRP {candidatePopup.lrp_idx + 1} candidate
              {candidatePopup.winner && <span className="cand-winner-badge"> ★ chosen</span>}
            </span>
            <button className="seg-info-close" onClick={() => { clearCandidatePopup(); setCandAnchor(null); candResetPos(); }}>✕</button>
          </header>
          <div className="seg-info-body">
            <CandidatePopupBody p={candidatePopup} />
          </div>
          {!candPos && <TipSvg placement={candPl} tipLeft={candTipLeft} />}
        </div>
        );
      })()}

      {/* FRC Legend — only shown when the Segs overlay is active */}
      {showSegmentLayer && (
        <div className="frc-legend">
          <h4>FRC</h4>
          {FRC_LABEL.map((label, i) => (
            <div key={i} className="legend-row">
              <div className="legend-swatch" style={{ background: FRC_COLOR[i] }} />
              <span>{label}</span>
            </div>
          ))}
        </div>
      )}

      {/* ── Encode mode: instructional banner (first-run guidance only) ────── */}
      {mode === 'encode' && waypoints.length === 0 && (
        <div className="encode-instruction-banner">
          {locationType === 'PointAlongLine'
            ? 'Right-click the map to place the point to encode.'
            : 'Right-click the map to start adding waypoints. When you’re happy with the route, expand the Results panel on the left to review it and encode.'}
        </div>
      )}

      {/* ── Encode mode: Clear/Undo ────────────────────────────────────────── */}
      {mode === 'encode' && (
        <div className="encode-tools">
          <button
            className="map-tool-btn"
            onClick={undoWaypoint}
            disabled={!waypointHistory.length}
            title="Undo last waypoint edit"
          >↺</button>
          <button
            className="map-tool-btn"
            onClick={clearWaypoints}
            disabled={!waypoints.length}
            title="Clear all waypoints"
          >🗑</button>
        </div>
      )}

      {/* ── Map tools toolbar ──────────────────────────────────────────────── */}

      {/* Toggle button — always visible */}
      <button
        className={`map-toolbar-toggle${toolbarOpen ? ' active' : ''}`}
        onClick={() => setToolbarOpen(v => !v)}
        title={toolbarOpen ? 'Hide tools' : 'Show tools'}
      >⚙</button>

      {/* Collapsible tool buttons */}
      <div className={`map-toolbar${toolbarOpen ? ' open' : ''}`}>
        <button
          className={`map-tool-btn${coordCaptureActive ? ' coord-capture-active' : ''}`}
          onClick={toggleCoordCapture}
          title={coordCaptureActive ? 'Cancel (Esc)' : 'Capture coordinates'}
        >📍</button>
        <button
          className={`map-tool-btn${showZoomPanel ? ' active' : ''}`}
          onClick={() => { setShowZoomPanel(v => !v); setZoomError(false); }}
          title="Zoom to coordinates"
        >🔍</button>
        <button
          className={`map-tool-btn${measuring ? ' active' : ''}`}
          onClick={toggleMeasure}
          title={measuring ? 'Cancel measurement (Esc)' : measurePts.length > 0 ? 'Clear measurement' : 'Measure distance'}
        >📏</button>
        <button
          className={`map-tool-btn${bearingActive ? ' active' : ''}`}
          onClick={toggleBearing}
          title={bearingActive ? 'Cancel bearing tool (Esc)' : 'Measure bearing and distance between two points'}
        >🧭</button>
        <button
          className={`map-tool-btn${permalinkCopied ? ' flash' : ''}`}
          onClick={doPermalink}
          title="Copy permalink to clipboard"
        >🔗</button>
      </div>

      {/* Tool panels — outside toolbar so they stay visible when toolbar collapses */}
      {coordCaptureActive && cursorCoord && (
        <div className="coord-display" title="Click map or press Enter to copy">
          {cursorCoord[1].toFixed(5)}, {cursorCoord[0].toFixed(5)}
        </div>
      )}
      {coordCopied && copiedText && (
        <div className="coord-display copied">✓ {copiedText}</div>
      )}

      {showZoomPanel && (
        <div className="zoomloc-panel">
          <input
            className={`zoomloc-input${zoomError ? ' error' : ''}`}
            placeholder="lat, lon"
            value={zoomInput}
            onChange={e => { setZoomInput(e.target.value); setZoomError(false); }}
            onKeyDown={e => e.key === 'Enter' && doZoomGo()}
            autoFocus
          />
          <button className="zoomloc-go" onClick={doZoomGo}>Go</button>
        </div>
      )}

      {(measuring || measurePts.length > 0) && (() => {
        const total = measurePts.reduce((sum, pt, i) =>
          i === 0 ? 0 : sum + haversineM(measurePts[i-1][0], measurePts[i-1][1], pt[0], pt[1]), 0);
        const pending = measuring && measureCursor && measurePts.length > 0
          ? haversineM(measurePts[measurePts.length-1][0], measurePts[measurePts.length-1][1], measureCursor[0], measureCursor[1])
          : null;
        return (
          <div className="measure-panel">
            {measurePts.length === 0 && <span className="measure-hint">Click to start</span>}
            {measurePts.length === 1 && pending == null && <span className="measure-hint">Click to add points</span>}
            {measurePts.length >= 2 && <span className="measure-total">{fmtDist(total)}</span>}
            {pending != null && (
              <span className="measure-pending">
                {measurePts.length >= 2 ? ' + ' : ''}{fmtDist(pending)}
                {measurePts.length >= 2 && <span className="measure-grand"> = {fmtDist(total + pending)}</span>}
              </span>
            )}
            {measuring && measurePts.length >= 1 && (
              <span className="measure-hint"> · dbl-click to finish</span>
            )}
          </div>
        );
      })()}

      {(bearingActive || bearingPts.length > 0) && (() => {
        const result = bearingPts.length === 2 ? (() => {
          const [p1, p2] = bearingPts;
          const dist = haversineM(p1[0], p1[1], p2[0], p2[1]);
          const bear = compassBearing(p1[0], p1[1], p2[0], p2[1]);
          return { dist, bear };
        })() : null;
        return (
          <div className="bearing-panel">
            {bearingPts.length === 0 && <span className="measure-hint">Click to set start point</span>}
            {bearingPts.length === 1 && <span className="measure-hint">Click to set end point</span>}
            {result && <>
              <span className="measure-total">{result.bear.toFixed(1)}°</span>
              <span className="bearing-sep"> · </span>
              <span className="measure-total">{fmtDist(result.dist)}</span>
              {bearingActive && <span className="measure-hint"> · click to remeasure</span>}
            </>}
          </div>
        );
      })()}

      {/* Basemap selector */}
      <div className="basemap-selector">
        <select value={basemap} onChange={e => handleBasemapChange(e.target.value)}>
          {BASEMAPS.map(b => (
            <option key={b.id} value={b.id}>{b.label}</option>
          ))}
        </select>
      </div>
    </div>
  );
}

// ── Candidate popup body ───────────────────────────────────────────────────────

function fmt(v, decimals = 2) {
  if (v == null) return '—';
  return typeof v === 'number' ? v.toFixed(decimals) : String(v);
}

/** Human-readable one-liner for a GateVerdict (serde externally-tagged). */
function formatVerdict(json) {
  if (!json) return null;
  let v;
  try { v = JSON.parse(json); } catch (_) { return null; }
  if (!v || v === 'Pass') return null;
  if (typeof v === 'string') return v;
  const key = Object.keys(v)[0];
  const val = v[key];
  switch (key) {
    case 'FailBearing':
      return `Bearing gate — exceeded by ${(val?.excess_deg ?? 0).toFixed(1)}°`;
    case 'FailRadius':
      return `Outside search radius`;
    case 'FailScore':
      return `Score too high${val?.score != null ? ` (${val.score.toFixed(4)})` : ''}`;
    case 'FailDirection':
      return `Wrong direction (one-way)`;
    default:
      return `${key}${typeof val === 'object' ? ': ' + JSON.stringify(val) : ''}`;
  }
}

const RESULT_LABEL = {
  accepted:  'Accepted',
  bearing:   'Bearing gate failed',
  radius:    'Outside search radius',
  score:     'Score gate failed',
  direction: 'Wrong direction',
  other:     'Rejected',
};

function CandidatePopupBody({ p }) {
  const accepted     = p.ctype === 'accepted';
  const resultLabel  = RESULT_LABEL[p.ctype] ?? p.ctype;
  const verdictLine  = !accepted ? formatVerdict(p.verdict_json) : null;
  const segKey       = p.stable_id ?? p.segment_id;

  return (
    <table className="cand-table">
      <tbody>
        {/* Verdict */}
        <tr>
          <td className="seg-info-key">Result</td>
          <td><b className={accepted ? 'cand-accepted' : 'cand-rejected'}>{resultLabel}</b></td>
        </tr>
        {verdictLine &&
          <tr><td className="seg-info-key"></td><td className="cand-verdict-detail">{verdictLine}</td></tr>}

        {/* Segment */}
        <tr><td colSpan={2} className="cand-section">Segment</td></tr>
        {segKey != null &&
          <tr><td className="seg-info-key">Key</td><td><b>{segKey}</b></td></tr>}
        {p.traversal &&
          <tr><td className="seg-info-key">Traversal</td><td><b>{p.traversal}</b></td></tr>}
        {p.frc_name != null &&
          <tr><td className="seg-info-key">FRC</td><td><b>{p.frc_name} ({p.frc})</b></td></tr>}
        {p.fow_name != null &&
          <tr><td className="seg-info-key">FOW</td><td><b>{p.fow_name} ({p.fow})</b></td></tr>}
        {p.direction != null &&
          <tr><td className="seg-info-key">Direction</td><td><b>{p.direction}</b></td></tr>}
        {p.length_m != null &&
          <tr><td className="seg-info-key">Length</td><td><b>{p.length_m} m</b></td></tr>}

        {/* Projection */}
        <tr><td colSpan={2} className="cand-section">Projection</td></tr>
        <tr><td className="seg-info-key">Dist from LRP</td><td><b>{fmt(p.distance_m)} m</b></td></tr>
        {p.arc_offset_m != null &&
          <tr><td className="seg-info-key">Arc offset</td><td><b>{fmt(p.arc_offset_m)} m</b></td></tr>}
        {p.bearing_deg != null &&
          <tr><td className="seg-info-key">Bearing</td><td><b>{fmt(p.bearing_deg, 1)}°</b></td></tr>}
        {p.snap_type != null && <tr><td className="seg-info-key">Snap type</td><td>
          <b className={`cand-snap-type cand-snap-${p.snap_type}`}>
            {p.snap_type === 'start' ? 'Start endpoint' : p.snap_type === 'end' ? 'End endpoint' : 'Interior'}
          </b>
        </td></tr>}
        {p.snap_lat != null &&
          <tr><td className="seg-info-key">Snap point</td><td><b style={{fontSize:'11px'}}>{Number(p.snap_lat).toFixed(6)}, {Number(p.snap_lon).toFixed(6)}</b></td></tr>}

        {/* Score breakdown */}
        <tr><td colSpan={2} className="cand-section">Score <span className="cand-lower">(lower = better)</span></td></tr>
        <tr><td className="seg-info-key">Total</td>     <td><b className="cand-score-total">{fmt(p.score_total, 4)}</b></td></tr>
        <tr><td className="seg-info-key">Distance</td>  <td><b>{fmt(p.score_distance, 4)}</b></td></tr>
        <tr><td className="seg-info-key">Bearing</td>   <td><b>{fmt(p.score_bearing, 4)}</b></td></tr>
        <tr><td className="seg-info-key">FRC</td>       <td><b>{fmt(p.score_frc, 4)}</b></td></tr>
        <tr><td className="seg-info-key">FOW</td>       <td><b>{fmt(p.score_fow, 4)}</b></td></tr>
        <tr><td className="seg-info-key">Wrong EP</td>  <td><b>{fmt(p.score_wrong_ep, 4)}</b></td></tr>
        <tr><td className="seg-info-key">Interior</td>  <td><b>{fmt(p.score_interior, 4)}</b></td></tr>
      </tbody>
    </table>
  );
}
