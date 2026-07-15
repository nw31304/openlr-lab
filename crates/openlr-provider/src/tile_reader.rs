//! Parse OLRL v3 binary tile payloads into the in-memory Graph.
//!
//! Binary layout (all integers little-endian):
//!
//! Header              40 bytes
//! Segment array       segment_count × 32 bytes
//! Geometry pool       geom_vertex_count × 8 bytes
//! Node table          node_count × 28 bytes
//! Intra restrictions  restriction_count × 16 bytes
//! Cross restrictions  xrestriction_count × 16 bytes
//! String pool         string_pool_length bytes
//!
//! Segment record (32 bytes):
//!   0..4   start_node u32
//!   4..8   end_node u32
//!   8..12  geom_offset u32
//!  12..14  geom_len u16
//!  14..18  length_cm u32
//!  18      attrs u8 (frc[2:0] | fow[5:3] | dir[7:6])
//!  19      flags u8
//!  20..24  stable_id_offset u32 (byte offset into string pool)
//!  24      stable_id_len u8
//!  25..32  _reserved
//!
//! Node record (28 bytes):
//!   0..4   lon_e7 i32
//!   4..8   lat_e7 i32
//!   8..12  stable_id_offset u32
//!  12      stable_id_len u8
//!  13..24  _reserved
//!  24      flags u8 (bit 0 = is_boundary)
//!  25..28  _pad
//!
//! Cross-restriction record (16 bytes):
//!   0..4   from_id_offset u32
//!   4      from_id_len u8
//!   5..9   via_node_local u32
//!   9..13  to_id_offset u32
//!  13      to_id_len u8
//!  14      flags u8
//!  15      _pad u8

use std::collections::HashMap;

use openlr_graph::{
    Direction, Graph, NetworkNode, NetworkSegment, NodeId, SegmentId, TurnRestriction,
};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TileReadError {
    #[error("bad magic: expected OLRL, got {0:?}")]
    BadMagic([u8; 4]),
    #[error("unsupported tile version {0} (expected 3)")]
    UnsupportedVersion(u8),
    #[error("tile payload too short: need at least {need} bytes, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("geometry index out of range: offset {offset} + len {len} > pool size {pool}")]
    GeomOutOfRange { offset: usize, len: usize, pool: usize },
    #[error("segment {index} has {len} geometry vertices (minimum 2)")]
    GeomTooShort { index: usize, len: usize },
    #[error("local node index {0} out of range")]
    NodeIndexOob(usize),
    #[error("local segment index {0} out of range")]
    SegIndexOob(usize),
    #[error("string pool reference out of range: offset {offset} + len {len} > pool size {pool}")]
    PoolOutOfRange { offset: usize, len: usize, pool: usize },
    #[error("string pool entry is not valid UTF-8")]
    InvalidUtf8,
}

// ── Cross-tile restriction pending entry ──────────────────────────────────────

/// A cross-tile turn restriction that cannot be fully resolved at parse time because
/// one or both segments live in a tile that may not yet be loaded.
/// `via_node` is always in the tile where the restriction was stored and is resolved
/// immediately; `from_id`/`to_id` are resolved in a post-load stitch pass.
#[derive(Debug, Clone)]
struct PendingXRestr {
    from_id:  String,
    via_node: NodeId,
    to_id:    String,
}

// ── Tile loader (multi-tile, boundary-node stitching) ─────────────────────────

/// Accumulates tiles into an in-memory `Graph`, stitching boundary nodes and
/// cross-tile turn restrictions across tiles.
pub struct TileLoader {
    pub graph: Graph,
    /// Stable source ID → global NodeId, universal dedup map (all nodes, not just boundary).
    boundary_nodes: HashMap<String, NodeId>,
    /// Stable source ID → global SegmentId, dedup map. A segment straddling a
    /// tile boundary is emitted into every tile it touches, so without this
    /// the same real-world edge would be re-parsed into a second, distinct
    /// `SegmentId` each time an overlapping tile loads — two "identical twin"
    /// segments (same stable_id/nodes/geometry/length) that `shortest_path`
    /// can't tell apart, so two otherwise-identical searches for the same
    /// edge can silently pick different twins and disagree with each other
    /// (e.g. route construction resolving one twin, then the encoder's own
    /// shortest-path reproduction check picking the other and failing to
    /// find the "same" path at all).
    boundary_segs: HashMap<String, SegmentId>,
    next_node_id: u32,
    next_seg_id: u32,
    /// Maps each SegmentId → (tile_z, tile_x, tile_y, local_segment_index_within_tile).
    pub seg_tile: HashMap<SegmentId, (u8, u32, u32, u32)>,
    /// Maps each (tile_z, tile_x, tile_y, local_node_index_within_tile) → global NodeId.
    /// Unlike `seg_tile` this is not invertible one-to-one: a boundary node's global
    /// NodeId appears once per tile that touches it, each at its own local index.
    pub node_tile: HashMap<(u8, u32, u32, u32), NodeId>,
    /// Cross-tile restrictions waiting for their from/to segments to be loaded.
    pending_xrestr: Vec<PendingXRestr>,
}

impl Default for TileLoader {
    fn default() -> Self { Self::new() }
}

impl TileLoader {
    pub fn new() -> Self {
        Self {
            graph: Graph::new(),
            boundary_nodes: HashMap::new(),
            boundary_segs: HashMap::new(),
            next_node_id: 0,
            next_seg_id: 0,
            seg_tile: HashMap::new(),
            node_tile: HashMap::new(),
            pending_xrestr: Vec::new(),
        }
    }

    /// Parse one OLRL v3 tile payload and merge it into the graph.
    /// Cross-tile restrictions are collected and stitched immediately against already-loaded
    /// segments; any that reference not-yet-loaded segments stay pending and will be resolved
    /// when the next tile is loaded.
    ///
    /// Returns the per-tile local node/segment index → global `NodeId`/`SegmentId` tables
    /// (see `load_tile_at`, which is how most callers get `node_tile`/`seg_tile` populated
    /// instead of using this directly).
    pub fn load_tile(&mut self, bytes: &[u8]) -> Result<(Vec<NodeId>, Vec<SegmentId>), TileReadError> {
        let (local_nodes, local_segs) = parse_tile(
            bytes,
            &mut self.graph,
            &mut self.boundary_nodes,
            &mut self.boundary_segs,
            &mut self.next_node_id,
            &mut self.next_seg_id,
            &mut self.pending_xrestr,
        )?;
        self.stitch_cross_tile();
        Ok((local_nodes, local_segs))
    }

    /// Like `load_tile`, but also records the tile key and local index for each
    /// ingested segment and node so callers can map a `SegmentId`/`NodeId` back to
    /// its tile origin.
    ///
    /// An empty `bytes` slice means the tile is not present in the archive.  The
    /// tile is still marked as loaded so A* does not keep requesting it — boundary
    /// nodes that home to this tile are treated as genuine dead ends.
    pub fn load_tile_at(&mut self, z: u8, x: u32, y: u32, bytes: &[u8]) -> Result<(), TileReadError> {
        if bytes.is_empty() {
            self.graph.mark_tile_loaded(z, x, y);
            return Ok(());
        }
        let (local_nodes, local_segs) = self.load_tile(bytes)?;
        // Index by the *returned* per-tile tables, not an assumed contiguous
        // ID range — a segment/node deduplicated against an earlier tile
        // reuses that tile's existing ID rather than getting a fresh one.
        for (local_idx, seg_id) in local_segs.into_iter().enumerate() {
            self.seg_tile.insert(seg_id, (z, x, y, local_idx as u32));
        }
        for (local_idx, node_id) in local_nodes.into_iter().enumerate() {
            self.node_tile.insert((z, x, y, local_idx as u32), node_id);
        }
        self.graph.mark_tile_loaded(z, x, y);
        Ok(())
    }

    /// Resolve pending cross-tile restrictions against currently loaded segments.
    /// Called automatically after each `load_tile`; exposed publicly so callers can
    /// trigger an extra pass after on-demand tile fetches if needed.
    pub fn stitch_cross_tile(&mut self) {
        if self.pending_xrestr.is_empty() { return; }

        // Build stable_id → Vec<SegmentId> reverse map over all loaded segments.
        let mut by_stable: HashMap<String, Vec<SegmentId>> = HashMap::new();
        for (&seg_id, seg) in &self.graph.segments {
            by_stable.entry(seg.stable_id.clone()).or_default().push(seg_id);
        }

        let pending = std::mem::take(&mut self.pending_xrestr);
        let mut still_pending = Vec::new();

        for p in pending {
            let froms = by_stable.get(p.from_id.as_str());
            let tos   = by_stable.get(p.to_id.as_str());

            match (froms, tos) {
                (Some(froms), Some(tos)) => {
                    for &from_seg in froms {
                        let fs = match self.graph.segments.get(&from_seg) { Some(s) => s, None => continue };
                        if fs.start_node != p.via_node && fs.end_node != p.via_node { continue; }
                        for &to_seg in tos {
                            let ts = match self.graph.segments.get(&to_seg) { Some(s) => s, None => continue };
                            if ts.start_node != p.via_node && ts.end_node != p.via_node { continue; }
                            self.graph.add_restriction(TurnRestriction { from_seg, via_node: p.via_node, to_seg });
                        }
                    }
                }
                _ => still_pending.push(p),
            }
        }

        self.pending_xrestr = still_pending;
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Returns the per-tile local node index → global `NodeId` table, so callers can
/// map a (tile, local_index) node reference back to its graph-wide identity.
fn parse_tile(
    b: &[u8],
    graph: &mut Graph,
    boundary_nodes: &mut HashMap<String, NodeId>,
    boundary_segs: &mut HashMap<String, SegmentId>,
    next_node: &mut u32,
    next_seg: &mut u32,
    pending_xrestr: &mut Vec<PendingXRestr>,
) -> Result<(Vec<NodeId>, Vec<SegmentId>), TileReadError> {
    require(b, 40)?;

    let magic: [u8; 4] = b[0..4].try_into().expect("slice is exactly 4 bytes");
    if &magic != b"OLRL" {
        return Err(TileReadError::BadMagic(magic));
    }
    if b[4] != 3 {
        return Err(TileReadError::UnsupportedVersion(b[4]));
    }

    let seg_count    = u32_le(b, 8)  as usize;
    let node_count   = u32_le(b, 12) as usize;
    let restr_count  = u32_le(b, 16) as usize;
    let geom_count   = u32_le(b, 20) as usize;
    let xrestr_count = u32_le(b, 24) as usize;
    let pool_length  = u32_le(b, 28) as usize;

    // Compute section offsets (all overflow-safe).
    let checked = (|| -> Option<(usize, usize, usize, usize, usize, usize, usize)> {
        let seg_off    = 40usize;
        let geom_off   = seg_off   .checked_add(seg_count  .checked_mul(32)?)?;
        let node_off   = geom_off  .checked_add(geom_count .checked_mul(8)?)?;
        let restr_off  = node_off  .checked_add(node_count .checked_mul(28)?)?;
        let xrestr_off = restr_off .checked_add(restr_count.checked_mul(16)?)?;
        let pool_off   = xrestr_off.checked_add(xrestr_count.checked_mul(16)?)?;
        let min_len    = pool_off  .checked_add(pool_length)?;
        Some((seg_off, geom_off, node_off, restr_off, xrestr_off, pool_off, min_len))
    })().ok_or(TileReadError::TooShort { need: usize::MAX, have: b.len() })?;
    let (seg_off, geom_off, node_off, restr_off, xrestr_off, pool_off, min_len) = checked;

    require(b, min_len)?;

    // ── String pool ──────────────────────────────────────────────────────────
    let pool = &b[pool_off..pool_off + pool_length];

    let read_pool_str = |off: usize, len: usize| -> Result<String, TileReadError> {
        if off + len > pool.len() {
            return Err(TileReadError::PoolOutOfRange { offset: off, len, pool: pool.len() });
        }
        std::str::from_utf8(&pool[off..off + len])
            .map(str::to_owned)
            .map_err(|_| TileReadError::InvalidUtf8)
    };

    // ── Geometry pool ────────────────────────────────────────────────────────
    let geom_pool: Vec<(f64, f64)> = (0..geom_count)
        .map(|i| {
            let o = geom_off + i * 8;
            let lon = i32_le(b, o)     as f64 / 1e7;
            let lat = i32_le(b, o + 4) as f64 / 1e7;
            (lon, lat)
        })
        .collect();

    // ── Node table ───────────────────────────────────────────────────────────
    let mut local_node: Vec<NodeId> = Vec::with_capacity(node_count);
    for i in 0..node_count {
        let o = node_off + i * 28;
        let lon         = i32_le(b, o)     as f64 / 1e7;
        let lat         = i32_le(b, o + 4) as f64 / 1e7;
        let sid_off     = u32_le(b, o + 8) as usize;
        let sid_len     = b[o + 12] as usize;
        let is_boundary = b[o + 24] & 0x01 != 0;

        let stable_id = read_pool_str(sid_off, sid_len)?;

        let node_id = *boundary_nodes.entry(stable_id.clone()).or_insert_with(|| {
            let id = NodeId(*next_node);
            *next_node += 1;
            id
        });

        if !graph.nodes.contains_key(&node_id) {
            graph.add_node(NetworkNode { id: node_id, lon, lat, stable_id, is_boundary });
        }
        local_node.push(node_id);
    }

    // ── Segment array ────────────────────────────────────────────────────────
    let mut local_seg: Vec<SegmentId> = Vec::with_capacity(seg_count);
    for i in 0..seg_count {
        let o = seg_off + i * 32;
        let start_local = u32_le(b, o)      as usize;
        let end_local   = u32_le(b, o + 4)  as usize;
        let geom_idx    = u32_le(b, o + 8)  as usize;
        let geom_len    = u16_le(b, o + 12) as usize;
        let length_cm   = u32_le(b, o + 14);
        let attrs       = b[o + 18];
        let sid_off     = u32_le(b, o + 20) as usize;
        let sid_len     = b[o + 24] as usize;

        if start_local >= node_count { return Err(TileReadError::NodeIndexOob(start_local)); }
        if end_local   >= node_count { return Err(TileReadError::NodeIndexOob(end_local)); }
        if geom_idx + geom_len > geom_count {
            return Err(TileReadError::GeomOutOfRange { offset: geom_idx, len: geom_len, pool: geom_count });
        }
        if geom_len < 2 {
            return Err(TileReadError::GeomTooShort { index: i, len: geom_len });
        }

        let frc = attrs & 0x07;
        let fow = (attrs >> 3) & 0x07;
        let direction = match (attrs >> 6) & 0x03 {
            1 => Direction::Forward,
            2 => Direction::Backward,
            _ => Direction::Both,
        };
        let geometry  = geom_pool[geom_idx..geom_idx + geom_len].to_vec();
        let stable_id = read_pool_str(sid_off, sid_len)?;

        let seg_id = *boundary_segs.entry(stable_id.clone()).or_insert_with(|| {
            let id = SegmentId(*next_seg);
            *next_seg += 1;
            id
        });
        local_seg.push(seg_id);

        if !graph.segments.contains_key(&seg_id) {
            graph.add_segment(NetworkSegment {
                id: seg_id,
                start_node: local_node[start_local],
                end_node:   local_node[end_local],
                geometry,
                length_m: length_cm as f64 / 100.0,
                frc,
                fow,
                direction,
                stable_id,
            });
        }
    }

    // ── Intra-tile restrictions ───────────────────────────────────────────────
    for i in 0..restr_count {
        let o = restr_off + i * 16;
        let from = u32_le(b, o)     as usize;
        let via  = u32_le(b, o + 4) as usize;
        let to   = u32_le(b, o + 8) as usize;

        if from >= seg_count  { return Err(TileReadError::SegIndexOob(from)); }
        if to   >= seg_count  { return Err(TileReadError::SegIndexOob(to)); }
        if via  >= node_count { return Err(TileReadError::NodeIndexOob(via)); }

        graph.add_restriction(TurnRestriction {
            from_seg: local_seg[from],
            via_node: local_node[via],
            to_seg:   local_seg[to],
        });
    }

    // ── Cross-tile restriction table ─────────────────────────────────────────
    for i in 0..xrestr_count {
        let o = xrestr_off + i * 16;
        let from_off  = u32_le(b, o)     as usize;
        let from_len  = b[o + 4]         as usize;
        let via_local = u32_le(b, o + 5) as usize;
        let to_off    = u32_le(b, o + 9) as usize;
        let to_len    = b[o + 13]        as usize;
        // flags at o+14, _pad at o+15 — reserved

        if via_local >= node_count { return Err(TileReadError::NodeIndexOob(via_local)); }

        let from_id = read_pool_str(from_off, from_len)?;
        let to_id   = read_pool_str(to_off, to_len)?;

        pending_xrestr.push(PendingXRestr {
            from_id,
            via_node: local_node[via_local],
            to_id,
        });
    }

    Ok((local_node, local_seg))
}

// ── Byte helpers ──────────────────────────────────────────────────────────────

fn require(b: &[u8], n: usize) -> Result<(), TileReadError> {
    if b.len() < n { Err(TileReadError::TooShort { need: n, have: b.len() }) } else { Ok(()) }
}

fn u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]])
}
fn i32_le(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]])
}
fn u16_le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o+1]])
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_reader::TileReadError;

    /// Build a minimal v3 tile payload (2 segments, 3 nodes) for testing.
    fn minimal_tile() -> Vec<u8> {
        // We use pipeline tile builder helpers directly by constructing the binary manually.
        let seg_count:   u32 = 2;
        let node_count:  u32 = 3;
        let restr_count: u32 = 0;
        let geom_count:  u32 = 4;
        let xrestr_count:u32 = 0;

        // String pool: seg0_id, seg1_id, node0_id, node1_id, node2_id
        let seg0_id  = "seg-0";
        let seg1_id  = "seg-1";
        let node0_id = "n-0";
        let node1_id = "n-1";
        let node2_id = "n-2";

        let mut pool: Vec<u8> = Vec::new();
        let seg0_off  = pool.len(); pool.extend_from_slice(seg0_id.as_bytes());
        let seg1_off  = pool.len(); pool.extend_from_slice(seg1_id.as_bytes());
        let nd0_off   = pool.len(); pool.extend_from_slice(node0_id.as_bytes());
        let nd1_off   = pool.len(); pool.extend_from_slice(node1_id.as_bytes());
        let nd2_off   = pool.len(); pool.extend_from_slice(node2_id.as_bytes());
        let pool_len  = pool.len() as u32;

        let mut buf: Vec<u8> = Vec::new();

        // Header (40 bytes)
        buf.extend_from_slice(b"OLRL");
        buf.push(3);                        // version 3
        buf.push(0);                        // flags
        buf.extend_from_slice(&[0u8; 2]);   // _pad
        buf.extend_from_slice(&seg_count.to_le_bytes());
        buf.extend_from_slice(&node_count.to_le_bytes());
        buf.extend_from_slice(&restr_count.to_le_bytes());
        buf.extend_from_slice(&geom_count.to_le_bytes());
        buf.extend_from_slice(&xrestr_count.to_le_bytes());
        buf.extend_from_slice(&pool_len.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);   // _reserved
        assert_eq!(buf.len(), 40);

        // Segment array (2 × 32 bytes)
        let attrs0: u8 = 3 | (3 << 3) | (0 << 6); // frc=3, fow=3, Both
        let attrs1: u8 = 3 | (3 << 3) | (1 << 6); // frc=3, fow=3, Forward
        for (start, end, geom_off, geom_len_v, attrs, sid_off, sid_len) in [
            (0u32, 1u32, 0u32, 2u16, attrs0, seg0_off as u32, seg0_id.len() as u8),
            (1u32, 2u32, 2u32, 2u16, attrs1, seg1_off as u32, seg1_id.len() as u8),
        ] {
            let mut s = [0u8; 32];
            s[0..4].copy_from_slice(&start.to_le_bytes());
            s[4..8].copy_from_slice(&end.to_le_bytes());
            s[8..12].copy_from_slice(&geom_off.to_le_bytes());
            s[12..14].copy_from_slice(&geom_len_v.to_le_bytes());
            s[14..18].copy_from_slice(&10_000u32.to_le_bytes()); // 100 m
            s[18] = attrs;
            s[20..24].copy_from_slice(&sid_off.to_le_bytes());
            s[24] = sid_len;
            buf.extend_from_slice(&s);
        }

        // Geometry pool (4 × 8 bytes)
        let lon0: i32 = 1_740_000_000;
        let lat0: i32 = -360_000_000;
        for lon in [lon0, lon0 + 10_000, lon0 + 10_000, lon0 + 20_000] {
            buf.extend_from_slice(&lon.to_le_bytes());
            buf.extend_from_slice(&lat0.to_le_bytes());
        }

        // Node table (3 × 28 bytes)
        for (lon, sid_off, sid_len) in [
            (lon0,          nd0_off as u32, node0_id.len() as u8),
            (lon0 + 10_000, nd1_off as u32, node1_id.len() as u8),
            (lon0 + 10_000, nd2_off as u32, node2_id.len() as u8),
        ] {
            buf.extend_from_slice(&lon.to_le_bytes());
            buf.extend_from_slice(&lat0.to_le_bytes());
            buf.extend_from_slice(&sid_off.to_le_bytes());
            buf.push(sid_len);
            buf.extend_from_slice(&[0u8; 11]); // _reserved
            buf.push(0); // flags: not boundary
            buf.extend_from_slice(&[0u8; 3]); // _pad
        }

        // No restrictions.
        // String pool
        buf.extend_from_slice(&pool);

        buf
    }

    #[test]
    fn parse_minimal_tile() {
        let bytes = minimal_tile();
        let mut loader = TileLoader::new();
        loader.load_tile(&bytes).unwrap();
        let g = &loader.graph;
        assert_eq!(g.segments.len(), 2, "segment count");
        assert_eq!(g.nodes.len(), 3, "node count");
    }

    #[test]
    fn segment_lengths_correct() {
        let bytes = minimal_tile();
        let mut loader = TileLoader::new();
        loader.load_tile(&bytes).unwrap();
        let lengths: std::collections::HashSet<u32> =
            loader.graph.segments.values().map(|s| s.length_m as u32).collect();
        assert!(lengths.contains(&100), "100 m segment");
    }

    #[test]
    fn direction_decoded_correctly() {
        let bytes = minimal_tile();
        let mut loader = TileLoader::new();
        loader.load_tile(&bytes).unwrap();
        let dirs: Vec<Direction> = loader.graph.segments.values().map(|s| s.direction).collect();
        assert!(dirs.contains(&Direction::Both));
        assert!(dirs.contains(&Direction::Forward));
    }

    #[test]
    fn stable_ids_decoded() {
        let bytes = minimal_tile();
        let mut loader = TileLoader::new();
        loader.load_tile(&bytes).unwrap();
        let ids: std::collections::HashSet<&str> = loader.graph.segments.values()
            .map(|s| s.stable_id.as_str()).collect();
        assert!(ids.contains("seg-0"));
        assert!(ids.contains("seg-1"));
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = minimal_tile();
        bytes[0] = b'X';
        assert!(matches!(TileLoader::new().load_tile(&bytes), Err(TileReadError::BadMagic(_))));
    }

    #[test]
    fn wrong_version_rejected() {
        let mut bytes = minimal_tile();
        bytes[4] = 2; // old version
        assert!(matches!(TileLoader::new().load_tile(&bytes), Err(TileReadError::UnsupportedVersion(2))));
    }

    fn node_off(tile: &[u8]) -> usize {
        // header(40) + segs(2×32) + geom(4×8)
        40 + 2 * 32 + 4 * 8
    }

    fn set_node_stable_id_and_flag(tile: &mut Vec<u8>, node_idx: usize, id: &str, is_boundary: bool) {
        // The string pool is at the end; to change a node's stable id we'd need to update the
        // pool and offsets.  For boundary-stitching tests we use a simpler approach: build a
        // fresh tile with known string pool positions.
        //
        // For these tests we manipulate the flags byte only (offset into the node record is
        // fixed at node_off + i*28 + 24).
        let _ = id; // deliberately unused; string pool is set at build time
        let o = node_off(tile) + node_idx * 28 + 24;
        tile[o] = u8::from(is_boundary);
    }

    /// Build a minimal tile with custom node stable IDs baked into the string pool.
    fn tile_with_nodes(node_ids: &[&str], boundary: &[bool]) -> Vec<u8> {
        assert_eq!(node_ids.len(), 3);

        let seg_count:   u32 = 2;
        let node_count:  u32 = 3;
        let geom_count:  u32 = 4;

        // Build string pool: 2 segment IDs + 3 node IDs
        let seg0_id = "s0";
        let seg1_id = "s1";
        let mut pool: Vec<u8> = Vec::new();
        let seg0_off = pool.len(); pool.extend_from_slice(seg0_id.as_bytes());
        let seg1_off = pool.len(); pool.extend_from_slice(seg1_id.as_bytes());
        let mut noff = Vec::new();
        for &id in node_ids {
            noff.push(pool.len() as u32);
            pool.extend_from_slice(id.as_bytes());
        }
        let pool_len = pool.len() as u32;

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"OLRL");
        buf.push(3); buf.push(0); buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&seg_count.to_le_bytes());
        buf.extend_from_slice(&node_count.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&geom_count.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&pool_len.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);

        for (start, end, g_off, s_off, s_len) in [
            (0u32, 1u32, 0u32, seg0_off as u32, seg0_id.len() as u8),
            (1u32, 2u32, 2u32, seg1_off as u32, seg1_id.len() as u8),
        ] {
            let mut s = [0u8; 32];
            s[0..4].copy_from_slice(&start.to_le_bytes());
            s[4..8].copy_from_slice(&end.to_le_bytes());
            s[8..12].copy_from_slice(&g_off.to_le_bytes());
            s[12..14].copy_from_slice(&2u16.to_le_bytes());
            s[14..18].copy_from_slice(&10_000u32.to_le_bytes());
            s[18] = 3 | (3 << 3);
            s[20..24].copy_from_slice(&s_off.to_le_bytes());
            s[24] = s_len;
            buf.extend_from_slice(&s);
        }

        let lon0: i32 = 1_740_000_000;
        let lat0: i32 = -360_000_000;
        for lon in [lon0, lon0 + 10_000, lon0 + 10_000, lon0 + 20_000] {
            buf.extend_from_slice(&lon.to_le_bytes());
            buf.extend_from_slice(&lat0.to_le_bytes());
        }

        for (i, (&nid_off, &id)) in noff.iter().zip(node_ids.iter()).enumerate() {
            let lon = lon0 + (i as i32) * 10_000;
            buf.extend_from_slice(&lon.to_le_bytes());
            buf.extend_from_slice(&lat0.to_le_bytes());
            buf.extend_from_slice(&nid_off.to_le_bytes());
            buf.push(id.len() as u8);
            buf.extend_from_slice(&[0u8; 11]);
            buf.push(u8::from(boundary[i]));
            buf.extend_from_slice(&[0u8; 3]);
        }

        buf.extend_from_slice(&pool);
        buf
    }

    #[test]
    fn boundary_nodes_stitched_across_tiles() {
        let tile1 = tile_with_nodes(&["n01", "n02", "shared"], &[false, false, true]);
        let tile2 = tile_with_nodes(&["shared", "n21", "n22"], &[true, false, false]);

        let mut loader = TileLoader::new();
        loader.load_tile(&tile1).unwrap();
        loader.load_tile(&tile2).unwrap();

        // tile1: 3 nodes; tile2 shares 1 → 5 unique
        assert_eq!(loader.graph.nodes.len(), 5, "boundary node stitched");
    }

    #[test]
    fn home_tile_node_stitches_with_foreign_occurrence() {
        // tile1: shared node is NOT boundary (home tile)
        let tile1 = tile_with_nodes(&["n01", "n02", "shared"], &[false, false, false]);
        // tile2: shared node IS boundary (foreign tile)
        let tile2 = tile_with_nodes(&["shared", "n21", "n22"], &[true, false, false]);

        let mut loader = TileLoader::new();
        loader.load_tile(&tile1).unwrap();
        loader.load_tile(&tile2).unwrap();

        assert_eq!(loader.graph.nodes.len(), 5,
            "home-tile non-boundary node must stitch with foreign-tile boundary occurrence");

        let shared_nodes: Vec<_> = loader.graph.nodes.values()
            .filter(|n| n.stable_id == "shared").collect();
        assert_eq!(shared_nodes.len(), 1, "shared node should appear exactly once");
    }

    /// Cross-tile restriction test using v3 16-byte records.
    #[test]
    fn cross_tile_restriction_stitched() {
        let from_id  = "from-seg";
        let to_id    = "to-seg";
        let via_id   = "via-node";

        // ── Tile 1: has via-node and the cross-tile restriction ──────────────
        let seg_count:   u32 = 2;
        let node_count:  u32 = 3;
        let geom_count:  u32 = 4;
        let xrestr_count:u32 = 1;

        // Segment IDs in tile1 (local segs, not from/to)
        let seg_a = "seg-a";
        let seg_b = "seg-b";
        let nd_a  = "nd-a";
        // via node shared across tiles
        let nd_c  = "nd-c";

        let mut pool: Vec<u8> = Vec::new();
        let sa_off = pool.len() as u32; pool.extend_from_slice(seg_a.as_bytes());
        let sb_off = pool.len() as u32; pool.extend_from_slice(seg_b.as_bytes());
        let na_off = pool.len() as u32; pool.extend_from_slice(nd_a.as_bytes());
        let _via_pool_off = pool.len() as u32; pool.extend_from_slice(via_id.as_bytes());
        let nc_off = pool.len() as u32; pool.extend_from_slice(nd_c.as_bytes());
        // Cross-restriction string pool entries
        let fr_off = pool.len() as u32; pool.extend_from_slice(from_id.as_bytes());
        let to_off_v = pool.len() as u32; pool.extend_from_slice(to_id.as_bytes());
        let pool_len = pool.len() as u32;

        let mut t1: Vec<u8> = Vec::new();
        t1.extend_from_slice(b"OLRL");
        t1.push(3); t1.push(0); t1.extend_from_slice(&[0u8; 2]);
        t1.extend_from_slice(&seg_count.to_le_bytes());
        t1.extend_from_slice(&node_count.to_le_bytes());
        t1.extend_from_slice(&0u32.to_le_bytes());
        t1.extend_from_slice(&geom_count.to_le_bytes());
        t1.extend_from_slice(&xrestr_count.to_le_bytes());
        t1.extend_from_slice(&pool_len.to_le_bytes());
        t1.extend_from_slice(&[0u8; 8]);

        for (start, end, g_off, s_off, s_len) in [
            (0u32, 1u32, 0u32, sa_off, seg_a.len() as u8),
            (1u32, 2u32, 2u32, sb_off, seg_b.len() as u8),
        ] {
            let mut s = [0u8; 32];
            s[0..4].copy_from_slice(&start.to_le_bytes());
            s[4..8].copy_from_slice(&end.to_le_bytes());
            s[8..12].copy_from_slice(&g_off.to_le_bytes());
            s[12..14].copy_from_slice(&2u16.to_le_bytes());
            s[14..18].copy_from_slice(&10_000u32.to_le_bytes());
            s[18] = 3 | (3 << 3);
            s[20..24].copy_from_slice(&s_off.to_le_bytes());
            s[24] = s_len;
            t1.extend_from_slice(&s);
        }

        let lon0: i32 = 1_740_000_000;
        let lat0: i32 = -360_000_000;
        for lon in [lon0, lon0 + 10_000, lon0 + 10_000, lon0 + 20_000] {
            t1.extend_from_slice(&lon.to_le_bytes());
            t1.extend_from_slice(&lat0.to_le_bytes());
        }

        // via_id is the node at local index 1
        let via_pool_off_actual = nd_a.len() as u32 + seg_a.len() as u32 + seg_b.len() as u32;
        for (lon, nid_off, nid_len, is_b) in [
            (lon0,          na_off,              nd_a.len() as u8,   false),
            (lon0 + 10_000, via_pool_off_actual, via_id.len() as u8, true),
            (lon0 + 20_000, nc_off,              nd_c.len() as u8,   false),
        ] {
            t1.extend_from_slice(&lon.to_le_bytes());
            t1.extend_from_slice(&lat0.to_le_bytes());
            t1.extend_from_slice(&nid_off.to_le_bytes());
            t1.push(nid_len);
            t1.extend_from_slice(&[0u8; 11]);
            t1.push(u8::from(is_b));
            t1.extend_from_slice(&[0u8; 3]);
        }

        // Cross-restriction record (16 bytes): via = local node 1
        t1.extend_from_slice(&fr_off.to_le_bytes());  // from_id_offset
        t1.push(from_id.len() as u8);                 // from_id_len
        t1.extend_from_slice(&1u32.to_le_bytes());    // via_node_local = 1
        t1.extend_from_slice(&to_off_v.to_le_bytes());// to_id_offset
        t1.push(to_id.len() as u8);                   // to_id_len
        t1.push(0);                                   // flags
        t1.push(0);                                   // _pad

        t1.extend_from_slice(&pool);

        // ── Tile 2: contains from_seg and to_seg ─────────────────────────────
        let t2 = {
            let from_sid = from_id;
            let to_sid   = to_id;
            let nd_x     = "nd-x";
            let nd_y     = "nd-y";

            let mut p: Vec<u8> = Vec::new();
            let fs_off = p.len() as u32; p.extend_from_slice(from_sid.as_bytes());
            let ts_off = p.len() as u32; p.extend_from_slice(to_sid.as_bytes());
            let nx_off = p.len() as u32; p.extend_from_slice(nd_x.as_bytes());
            let via_off = p.len() as u32; p.extend_from_slice(via_id.as_bytes());
            let ny_off = p.len() as u32; p.extend_from_slice(nd_y.as_bytes());
            let p_len  = p.len() as u32;

            let mut t: Vec<u8> = Vec::new();
            t.extend_from_slice(b"OLRL");
            t.push(3); t.push(0); t.extend_from_slice(&[0u8; 2]);
            t.extend_from_slice(&2u32.to_le_bytes()); // seg
            t.extend_from_slice(&3u32.to_le_bytes()); // node
            t.extend_from_slice(&0u32.to_le_bytes()); // restr
            t.extend_from_slice(&4u32.to_le_bytes()); // geom
            t.extend_from_slice(&0u32.to_le_bytes()); // xrestr
            t.extend_from_slice(&p_len.to_le_bytes());
            t.extend_from_slice(&[0u8; 8]);

            // seg X→B (from_seg)  seg B→Y (to_seg)
            for (start, end, g_off, s_off, s_len) in [
                (0u32, 1u32, 0u32, fs_off, from_sid.len() as u8),
                (1u32, 2u32, 2u32, ts_off, to_sid.len() as u8),
            ] {
                let mut s = [0u8; 32];
                s[0..4].copy_from_slice(&start.to_le_bytes());
                s[4..8].copy_from_slice(&end.to_le_bytes());
                s[8..12].copy_from_slice(&g_off.to_le_bytes());
                s[12..14].copy_from_slice(&2u16.to_le_bytes());
                s[14..18].copy_from_slice(&10_000u32.to_le_bytes());
                s[18] = 3 | (3 << 3);
                s[20..24].copy_from_slice(&s_off.to_le_bytes());
                s[24] = s_len;
                t.extend_from_slice(&s);
            }

            let lon1: i32 = 1_750_000_000;
            for lon in [lon1, lon1 + 10_000, lon1 + 10_000, lon1 + 20_000] {
                t.extend_from_slice(&lon.to_le_bytes());
                t.extend_from_slice(&lat0.to_le_bytes());
            }

            for (lon, nid_off, nid_len, is_b) in [
                (lon1,          nx_off,  nd_x.len() as u8,   false),
                (lon1 + 10_000, via_off, via_id.len() as u8, true),
                (lon1 + 20_000, ny_off,  nd_y.len() as u8,   false),
            ] {
                t.extend_from_slice(&lon.to_le_bytes());
                t.extend_from_slice(&lat0.to_le_bytes());
                t.extend_from_slice(&nid_off.to_le_bytes());
                t.push(nid_len);
                t.extend_from_slice(&[0u8; 11]);
                t.push(u8::from(is_b));
                t.extend_from_slice(&[0u8; 3]);
            }

            t.extend_from_slice(&p);
            t
        };

        let mut loader = TileLoader::new();
        loader.load_tile(&t1).unwrap();
        assert_eq!(loader.pending_xrestr.len(), 1, "restriction pending after tile 1");
        assert_eq!(loader.graph.restrictions_count(), 0);

        loader.load_tile(&t2).unwrap();
        assert_eq!(loader.pending_xrestr.len(), 0, "no remaining pending restrictions");
        assert!(loader.graph.restrictions_count() > 0, "restriction stitched");
    }
}
