/// Opaque stable segment identifier (tile-local index at runtime;
/// resolved from GERS id during tile load).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct SegmentId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    Both,
    Forward,
    Backward,
}

/// A post-split, node-to-node road segment as stored in a loaded tile.
#[derive(Debug, Clone)]
pub struct NetworkSegment {
    pub id: SegmentId,
    pub start_node: super::NodeId,
    pub end_node: super::NodeId,
    /// Ordered WGS84 vertices (longitude, latitude).
    pub geometry: Vec<(f64, f64)>,
    /// Precomputed length in meters (do not re-derive from stored geometry).
    pub length_m: f64,
    pub frc: u8,
    pub fow: u8,
    pub direction: Direction,
    /// Stable 16-byte source ID:
    /// - OSM tiles:      encode_way_id(osm_way_id) — bytes 0-7 = way id LE, bytes 8-15 = 0
    /// - Overture tiles: full GERS UUID (128-bit, little-endian bytes)
    /// - Synthetic/test: [0u8; 16]
    pub stable_id: [u8; 16],
}

impl NetworkSegment {
    /// Decode the OSM way ID from `stable_id`, if this segment came from an OSM tile.
    ///
    /// OSM way IDs are encoded as `i64::to_le_bytes()` in bytes 0–7 with bytes 8–15
    /// all zero (see `encode_way_id` in the pipeline).  Returns `None` for Overture
    /// segments (where all 16 bytes are non-zero) and for synthetic test segments.
    pub fn osm_way_id(&self) -> Option<i64> {
        if self.stable_id[8..16] == [0u8; 8] && self.stable_id != [0u8; 16] {
            Some(i64::from_le_bytes(self.stable_id[0..8].try_into().unwrap()))
        } else {
            None
        }
    }
}
