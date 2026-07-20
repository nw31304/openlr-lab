// Shared formatting/labeling for the "Reference" (location-reference-point)
// display used by both ResultPanel.jsx and TracePanel.jsx. Kept in one place
// so wording (units, precision, range notation, FRC/FOW/orientation labels)
// can't silently drift between the two panels the way it had before this
// was consolidated -- e.g. "TomTomV3 (binary v3)" vs "TomTom v3" for the
// same format, or DNP/LFRCNP shown on the last LRP in one panel but not the
// other (that field is meaningless there -- no "next point" after the last).

export const FRC_LABEL = [
  'FRC0 · Motorway', 'FRC1 · Trunk', 'FRC2 · Secondary', 'FRC3 · Tertiary',
  'FRC4 · Unclassified', 'FRC5 · Residential', 'FRC6 · Service/Link', 'FRC7 · Other/Path',
];

export const FOW_LABEL = [
  'Undefined', 'Motorway', 'Dual Carriageway', 'Single Carriageway',
  'Roundabout', 'Traffic Square', 'Slip Road', 'Other',
];

export const ORIENTATION_LABEL = {
  NoOrientation:     'No orientation',
  FirstTowardSecond: 'First → Second',
  SecondTowardFirst: 'Second → First',
  BothDirections:    'Both directions',
};

export const SIDE_OF_ROAD_LABEL = {
  DirectlyOnOrNA: 'Directly on / N/A',
  Right:          'Right',
  Left:           'Left',
  Both:           'Both sides',
};

export function isPointAlongLine(locationType) {
  return locationType === 'PointAlongLine' || locationType === 'PoiWithAccessPoint';
}

export function formatOpenlrFormat(format) {
  if (format === 'TomTomV3') return 'TomTomV3 (binary v3)';
  if (format === 'Tpeg')     return 'TPEG-OLR (ISO 21219-22)';
  return '(unknown)';
}

export function frcLabel(frc) {
  return FRC_LABEL[frc] ?? `FRC${frc}`;
}

export function fowLabel(fow) {
  return FOW_LABEL[fow] != null ? `FOW${fow} · ${FOW_LABEL[fow]}` : `FOW${fow}`;
}

export function fmtBearing(lb, ub) {
  return Math.abs(ub - lb) < 0.1 ? `${lb.toFixed(1)}°` : `${lb.toFixed(1)}°–${ub.toFixed(1)}°`;
}

// General meter-range formatter (DNP, offsets) -- null only means "no data",
// callers that need to suppress a legitimate all-zero interval (e.g. "no
// offset configured") gate on that themselves before calling this.
export function fmtInterval(lb, ub) {
  if (lb == null) return null;
  return Math.abs(ub - lb) < 0.1 ? `${lb.toFixed(0)} m` : `${lb.toFixed(0)}–${ub.toFixed(0)} m`;
}

export function fmtOffsetValue(lb, ub, approximate) {
  const str = fmtInterval(lb, ub);
  if (str == null) return null;
  return approximate ? `${str} *` : str;
}

// For Line locations, the Pos/Neg Offset rows are always shown (explicitly
// "N/A" when the reference didn't encode one) rather than the row silently
// disappearing -- makes clear the field was considered, not just missing
// from the display.
export function offsetRowValue(hasValue, lb, ub, approximate) {
  return hasValue ? fmtOffsetValue(lb, ub, approximate) : 'N/A';
}

// Compact form for collapsed/summary rows -- just the number(s), no label text.
export function lfrcnpCompact(lfrcnp, tolerance = 0) {
  if (lfrcnp == null) return '—';
  return tolerance > 0 ? `${lfrcnp} → ${Math.min(lfrcnp + tolerance, 7)}` : `${lfrcnp}`;
}

// Full descriptive form for expanded/detail rows.
export function lfrcnpFull(lfrcnp, tolerance = 0) {
  if (lfrcnp == null) return '—';
  if (tolerance > 0) {
    return `${frcLabel(lfrcnp)} → ${frcLabel(Math.min(lfrcnp + tolerance, 7))}`;
  }
  return frcLabel(lfrcnp);
}
