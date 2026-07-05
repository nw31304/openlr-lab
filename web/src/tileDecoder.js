/**
 * Decode an OpenLRLens v3 tile payload (custom binary format) into GeoJSON features.
 *
 * Layout:
 *   Header              40 bytes
 *   Segment array       segment_count × 32 bytes
 *   Geometry pool       geom_vertex_count × 8 bytes   (lon_e7: i32 LE, lat_e7: i32 LE)
 *   Node table          node_count × 28 bytes
 *   Intra restrictions  restriction_count × 16 bytes
 *   Cross restrictions  xrestriction_count × 16 bytes
 *   String pool         string_pool_length bytes (UTF-8 stable IDs)
 *
 * Segment record (32 bytes):
 *   bytes  0..4  start_node u32
 *   bytes  4..8  end_node u32
 *   bytes  8..12 geom_offset u32
 *   bytes 12..14 geom_len u16
 *   bytes 14..18 length_cm u32
 *   byte  18     packed (frc bits[0..2], fow bits[3..5], direction bits[6..7])
 *   byte  19     reserved
 *   bytes 20..24 stable_id_offset u32  (into string pool)
 *   byte  24     stable_id_len u8
 *   bytes 25..32 reserved
 *
 * Node record (28 bytes):
 *   bytes  0..4  lon_e7 i32
 *   bytes  4..8  lat_e7 i32
 *   bytes  8..12 stable_id_offset u32
 *   byte  12     stable_id_len u8
 *   bytes 13..24 reserved
 *   byte  24     flags u8 (bit 0 = boundary node)
 *   bytes 25..28 pad
 *
 * Packed attribute byte: bits[0..2] = frc, bits[3..5] = fow, bits[6..7] = direction
 *   direction: 0=Both  1=Forward  2=Backward (0 also used as default)
 */

const MAGIC = 0x4C524C4F; // "OLRL" as little-endian u32

const DIRECTION = ['Both', 'Forward', 'Backward'];
const FRC_NAME  = ['Motorway', 'Trunk/Primary', 'Secondary', 'Tertiary',
                   'Unclassified', 'Residential', 'Living Street/Service', 'Other'];
const FOW_NAME  = ['Undefined', 'Motorway', 'Dual Carriageway', 'Single Carriageway',
                   'Roundabout', 'Traffic Square', 'Slip Road', 'Other'];

const utf8 = new TextDecoder();

export function decodeTile(buffer, z, x, y) {
  const view = new DataView(buffer);
  const bytes = new Uint8Array(buffer);

  if (view.getUint32(0, true) !== MAGIC) {
    console.warn(`tile ${z}/${x}/${y}: bad magic`);
    return { type: 'FeatureCollection', features: [] };
  }
  const version = view.getUint8(4);
  if (version !== 3) {
    console.warn(`tile ${z}/${x}/${y}: unsupported version ${version}, expected 3`);
  }

  const segmentCount      = view.getUint32(8,  true);
  const nodeCount         = view.getUint32(12, true);
  const restrictionCount  = view.getUint32(16, true);
  const geomVertexCount   = view.getUint32(20, true);
  const xrestrictionCount = view.getUint32(24, true);
  // string_pool_length at bytes 28..32 (informational; we compute the offset independently)

  // Section offsets
  const offSegArray   = 40;
  const offGeom       = offSegArray + segmentCount * 32;
  const offNodes      = offGeom     + geomVertexCount * 8;
  const offIntra      = offNodes    + nodeCount * 28;
  const offCross      = offIntra    + restrictionCount * 16;
  const offStringPool = offCross    + xrestrictionCount * 16;

  function readStableId(offset, len) {
    if (len === 0) return null;
    return utf8.decode(bytes.subarray(offStringPool + offset, offStringPool + offset + len));
  }

  const tileKey = `${z}/${x}/${y}`;
  const features = [];

  for (let i = 0; i < segmentCount; i++) {
    const base = offSegArray + i * 32;

    const startNode       = view.getUint32(base + 0,  true);
    const endNode         = view.getUint32(base + 4,  true);
    const geomOffset      = view.getUint32(base + 8,  true);
    const geomLen         = view.getUint16(base + 12, true);
    const lengthCm        = view.getUint32(base + 14, true);
    const packed          = view.getUint8(base + 18);
    const stableIdOffset  = view.getUint32(base + 20, true);
    const stableIdLen     = view.getUint8(base + 24);

    const frc    = packed & 0x07;
    const fow    = (packed >> 3) & 0x07;
    const dirIdx = (packed >> 6) & 0x03;

    const stable_id = readStableId(stableIdOffset, stableIdLen);

    const coords = [];
    for (let v = 0; v < geomLen; v++) {
      const vBase = offGeom + (geomOffset + v) * 8;
      coords.push([view.getInt32(vBase, true) * 1e-7, view.getInt32(vBase + 4, true) * 1e-7]);
    }
    if (coords.length < 2) continue;

    features.push({
      type: 'Feature',
      id: i,
      geometry: { type: 'LineString', coordinates: coords },
      properties: {
        frc,
        fow,
        direction:   DIRECTION[dirIdx] ?? 'Both',
        frc_name:    FRC_NAME[frc]  ?? String(frc),
        fow_name:    FOW_NAME[fow]  ?? String(fow),
        length_m:    (lengthCm / 100).toFixed(1),
        tile:        tileKey,
        local_index: i,
        stable_id,
        start_node:  startNode,
        end_node:    endNode,
      },
    });
  }

  const nodeFeatures = [];
  for (let i = 0; i < nodeCount; i++) {
    const base           = offNodes + i * 28;
    const lonE7          = view.getInt32(base + 0, true);
    const latE7          = view.getInt32(base + 4, true);
    const stableIdOffset = view.getUint32(base + 8, true);
    const stableIdLen    = view.getUint8(base + 12);
    const flags          = view.getUint8(base + 24);

    const stable_id  = readStableId(stableIdOffset, stableIdLen);
    const isBoundary = (flags & 0x01) !== 0;

    nodeFeatures.push({
      type: 'Feature',
      id:   i,
      geometry: { type: 'Point', coordinates: [lonE7 * 1e-7, latE7 * 1e-7] },
      properties: {
        stable_id,
        lat:         (latE7 * 1e-7).toFixed(7),
        lon:         (lonE7 * 1e-7).toFixed(7),
        local_index: i,
        tile:        tileKey,
        is_boundary: isBoundary,
      },
    });
  }

  return { type: 'FeatureCollection', features, nodeFeatures };
}
