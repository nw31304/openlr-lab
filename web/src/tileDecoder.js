/**
 * Decode an OpenLRLens v2 tile payload (custom binary format) into GeoJSON features.
 *
 * Layout:
 *   Header            40 bytes
 *   Segment array     segment_count × 32 bytes
 *   Seg GERS-id table segment_count × 16 bytes
 *   Geometry pool     geom_vertex_count × 8 bytes   (lon_e7: i32 LE, lat_e7: i32 LE)
 *   Node table        node_count × 28 bytes
 *   Intra restrictions restriction_count × 16 bytes
 *   Cross restrictions xrestriction_count × 40 bytes
 *
 * Packed attribute byte: bits[0..2] = frc, bits[3..5] = fow, bits[6..7] = direction
 *   direction: 0=Both  1=Forward  2=Backward
 */

const MAGIC = 0x4C524C4F; // "OLRL" as little-endian u32

const DIRECTION = ['Both', 'Forward', 'Backward'];
const FRC_NAME  = ['Motorway', 'Trunk/Primary', 'Secondary', 'Tertiary',
                   'Unclassified', 'Residential', 'Living Street/Service', 'Other'];
const FOW_NAME  = ['Undefined', 'Motorway', 'Dual Carriageway', 'Single Carriageway',
                   'Roundabout', 'Traffic Square', 'Slip Road', 'Other'];

export function decodeTile(buffer, z, x, y) {
  const view = new DataView(buffer);

  // Validate magic
  if (view.getUint32(0, true) !== MAGIC) {
    console.warn(`tile ${z}/${x}/${y}: bad magic`);
    return { type: 'FeatureCollection', features: [] };
  }
  const version = view.getUint8(4);
  if (version < 2) {
    console.warn(`tile ${z}/${x}/${y}: version ${version} < 2, GERS-id table absent`);
  }

  const segmentCount    = view.getUint32(8,  true);
  const nodeCount       = view.getUint32(12, true);
  const restrictionCount = view.getUint32(16, true);
  const geomVertexCount = view.getUint32(20, true);

  // Section offsets
  const offSegArray = 40;
  const offSegGers  = offSegArray + segmentCount * 32;
  const offGeom     = offSegGers  + segmentCount * 16;
  const offNodes    = offGeom     + geomVertexCount * 8;

  const tileKey = `${z}/${x}/${y}`;
  const features = [];

  for (let i = 0; i < segmentCount; i++) {
    const base = offSegArray + i * 32;

    const geomOffset = view.getUint32(base + 8,  true); // vertex index
    const geomLen    = view.getUint16(base + 12, true); // vertex count
    const lengthCm   = view.getUint32(base + 14, true);
    const packed     = view.getUint8(base + 18);

    const frc       = packed & 0x07;
    const fow       = (packed >> 3) & 0x07;
    const dirIdx    = (packed >> 6) & 0x03;

    // Source integer ID from stable-ID table (bytes 0-7 = source integer LE i64,
    // bytes 8-15 = 0 for simple integer IDs; full GERS UUIDs have non-zero high bytes).
    let source_id = null;
    const sidBase = offSegGers + i * 16;
    let isIntId = true;
    for (let b = 8; b < 16; b++) {
      if (view.getUint8(sidBase + b) !== 0) { isIntId = false; break; }
    }
    if (isIntId) {
      const idBig = view.getBigInt64(sidBase, true);
      if (idBig !== 0n) source_id = Number(idBig);
    }

    // Read geometry
    const coords = [];
    for (let v = 0; v < geomLen; v++) {
      const vBase = offGeom + (geomOffset + v) * 8;
      const lonE7 = view.getInt32(vBase,     true);
      const latE7 = view.getInt32(vBase + 4, true);
      coords.push([lonE7 * 1e-7, latE7 * 1e-7]);
    }
    if (coords.length < 2) continue;

    features.push({
      type: 'Feature',
      id: i,
      geometry: { type: 'LineString', coordinates: coords },
      properties: {
        frc,
        fow,
        direction: DIRECTION[dirIdx] ?? 'Both',
        frc_name:  FRC_NAME[frc]  ?? String(frc),
        fow_name:  FOW_NAME[fow]  ?? String(fow),
        length_m:  (lengthCm / 100).toFixed(1),
        tile:      tileKey,
        local_index: i,
        source_id,
      },
    });
  }

  return { type: 'FeatureCollection', features };
}
