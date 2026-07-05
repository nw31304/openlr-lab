use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rayon::prelude::*;
use tracing::{trace, warn};

use crate::adapt::AdaptedSegment;
use openlr_graph::Direction;

// ── Output types ──────────────────────────────────────────────────────────────

/// One node-to-node directed edge, produced by splitting a segment at
/// every interior connector position (Invariant 1).
#[derive(Debug, Clone)]
pub struct SplitEdge {
    pub start_node_id: [u8; 16],
    pub end_node_id: [u8; 16],
    pub geometry: Vec<(f64, f64)>, // (lon, lat), first = start node, last = end node
    pub length_m: f64,
    pub frc: u8,
    pub fow: u8,
    pub direction: Direction,
    /// The parent segment's internal binary key, used for turn-restriction cross-references.
    pub parent_id: [u8; 16],
    /// Zero-based index of this sub-edge among all sub-edges of the same parent segment.
    pub split_idx: u32,
}

/// One road node, de-duplicated by internal binary key across all edges.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub node_id: [u8; 16],
    pub lon: f64,
    pub lat: f64,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Split each segment at its interior connectors, but only where the connecting segment
/// is also vehicular.  `vehicular_endpoints` is the set of connector IDs that appear as
/// endpoints (at ≈ 0 or at ≈ 1) on vehicular segments — i.e. genuine road junctions.
/// Interior connectors absent from this set (footpath crossings, driveways, etc.) are skipped.
pub fn split(
    segments: Vec<AdaptedSegment>,
    vehicular_endpoints: &HashSet<String>,
) -> (Vec<SplitEdge>, Vec<NodeRecord>) {
    // Process segments in parallel; collect (edges, node-map-fragments) then merge.
    let results: Vec<(Vec<SplitEdge>, Vec<NodeRecord>)> = segments
        .into_par_iter()
        .map(|seg| split_segment(seg, vehicular_endpoints))
        .collect();

    let mut edges: Vec<SplitEdge> = Vec::new();
    // Deduplicate nodes by internal binary key; last writer wins (coordinates should agree).
    let mut node_map: HashMap<[u8; 16], NodeRecord> = HashMap::new();

    for (seg_edges, seg_nodes) in results {
        edges.extend(seg_edges);
        for n in seg_nodes {
            node_map.insert(n.node_id, n);
        }
    }

    let nodes: Vec<NodeRecord> = node_map.into_values().collect();
    (edges, nodes)
}

// ── Per-segment splitting ─────────────────────────────────────────────────────

fn split_segment(seg: AdaptedSegment, vehicular_endpoints: &HashSet<String>) -> (Vec<SplitEdge>, Vec<NodeRecord>) {
    if seg.geometry.len() < 2 {
        warn!(id = %seg.stable_id, "segment has fewer than 2 geometry points, skipped");
        return (vec![], vec![]);
    }

    // Parse the parent stable id once; skip the whole segment if it's malformed.
    let parent_id = match parse_hex_id(&seg.stable_id) {
        Ok(id) => id,
        Err(e) => {
            warn!(id = %seg.stable_id, error = %e, "segment has invalid hex id, skipped");
            return (vec![], vec![]);
        }
    };

    // Keep endpoint connectors (at ≈ 0 or ≈ 1) always; keep interior connectors only
    // when they are endpoints of other vehicular segments (genuine road junctions).
    let filtered_connectors: Vec<_> = seg.connectors.iter()
        .filter(|c| {
            let is_own_endpoint = c.at <= 1e-9 || c.at >= 1.0 - 1e-9;
            is_own_endpoint || vehicular_endpoints.contains(&c.connector_id)
        })
        .cloned()
        .collect();

    // We need at least connectors at at=0.0 and at=1.0 to form an edge.
    let connectors = &filtered_connectors;
    if connectors.is_empty() {
        warn!(id = %seg.stable_id, "segment has no connectors, skipped");
        return (vec![], vec![]);
    }

    // Clamp connectors to [0, 1] and ensure we have endpoints.
    let has_start = connectors.first().map(|c| c.at <= 1e-9).unwrap_or(false);
    let has_end   = connectors.last().map(|c| c.at >= 1.0 - 1e-9).unwrap_or(false);

    if !has_start || !has_end {
        warn!(
            id = %seg.stable_id,
            has_start,
            has_end,
            "segment missing endpoint connector, skipped"
        );
        return (vec![], vec![]);
    }

    let cum = cumulative_lengths(&seg.geometry);

    let mut edges = Vec::new();
    let mut node_records: Vec<NodeRecord> = Vec::new();
    let mut split_idx: u32 = 0;

    for pair in connectors.windows(2) {
        let c_start = &pair[0];
        let c_end   = &pair[1];

        // Skip zero-length spans (duplicate at values).
        if (c_end.at - c_start.at).abs() < 1e-9 {
            continue;
        }

        // Skip edges with malformed connector IDs rather than colliding on zero.
        let start_id = match parse_hex_id(&c_start.connector_id) {
            Ok(id) => id,
            Err(e) => {
                warn!(id = %seg.stable_id, connector = %c_start.connector_id,
                      error = %e, "invalid start connector id, edge skipped");
                continue;
            }
        };
        let end_id = match parse_hex_id(&c_end.connector_id) {
            Ok(id) => id,
            Err(e) => {
                warn!(id = %seg.stable_id, connector = %c_end.connector_id,
                      error = %e, "invalid end connector id, edge skipped");
                continue;
            }
        };

        let sub_geom = sub_geometry(&seg.geometry, &cum, c_start.at, c_end.at);
        if sub_geom.len() < 2 {
            warn!(id = %seg.stable_id, at_start = c_start.at, at_end = c_end.at,
                  "sub-geometry degenerate, skipped");
            continue;
        }

        let length_m = polyline_length_m(&sub_geom);
        trace!(id = %seg.stable_id, at_start = c_start.at, at_end = c_end.at,
               length_m, "split edge");

        node_records.push(NodeRecord {
            node_id: start_id,
            lon: sub_geom[0].0,
            lat: sub_geom[0].1,
        });
        node_records.push(NodeRecord {
            node_id: end_id,
            lon: sub_geom.last().unwrap().0,
            lat: sub_geom.last().unwrap().1,
        });

        edges.push(SplitEdge {
            start_node_id: start_id,
            end_node_id:   end_id,
            geometry:      sub_geom,
            length_m,
            frc:           seg.frc,
            fow:           seg.fow,
            direction:     seg.direction,
            parent_id,
            split_idx,
        });
        split_idx += 1;
    }

    (edges, node_records)
}

// ── Geometry helpers ──────────────────────────────────────────────────────────

/// Haversine distance in metres between two WGS84 points (lon, lat in degrees).
pub fn haversine_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

/// Cumulative arc-lengths along a polyline.  cum[0] == 0, cum[n-1] == total length.
fn cumulative_lengths(geometry: &[(f64, f64)]) -> Vec<f64> {
    let mut cum = vec![0.0f64; geometry.len()];
    for i in 1..geometry.len() {
        cum[i] = cum[i - 1]
            + haversine_m(geometry[i - 1].0, geometry[i - 1].1, geometry[i].0, geometry[i].1);
    }
    cum
}

/// Sum of Haversine distances along a polyline.
pub(crate) fn polyline_length_m(geom: &[(f64, f64)]) -> f64 {
    geom.windows(2)
        .map(|w| haversine_m(w[0].0, w[0].1, w[1].0, w[1].1))
        .sum()
}

/// Interpolate the polyline at arc-length `arc_target` (metres from the start).
fn interp_at_arc(geometry: &[(f64, f64)], cum: &[f64], arc_target: f64) -> (f64, f64) {
    let arc_target = arc_target.max(0.0).min(*cum.last().unwrap());

    for i in 1..geometry.len() {
        if cum[i] >= arc_target - 1e-9 {
            let seg_len = cum[i] - cum[i - 1];
            if seg_len < 1e-12 {
                return geometry[i - 1];
            }
            let t = ((arc_target - cum[i - 1]) / seg_len).clamp(0.0, 1.0);
            let (x0, y0) = geometry[i - 1];
            let (x1, y1) = geometry[i];
            return (x0 + t * (x1 - x0), y0 + t * (y1 - y0));
        }
    }
    *geometry.last().unwrap()
}

/// Extract the sub-polyline from fraction `t_start` to `t_end` (both in [0, 1]).
/// The returned geometry starts at the interpolated point for `t_start` and ends at
/// the interpolated point for `t_end`, with all original vertices in between retained.
fn sub_geometry(
    geometry: &[(f64, f64)],
    cum: &[f64],
    t_start: f64,
    t_end: f64,
) -> Vec<(f64, f64)> {
    let total = *cum.last().unwrap();
    if total < 1e-12 {
        // Zero-length segment — return as-is.
        return geometry.to_vec();
    }

    let arc_start = (t_start * total).max(0.0);
    let arc_end   = (t_end   * total).min(total);

    let mut pts = Vec::new();

    // Interpolated start point.
    pts.push(interp_at_arc(geometry, cum, arc_start));

    // Original vertices strictly between the two arc positions.
    for i in 1..geometry.len().saturating_sub(1) {
        if cum[i] > arc_start + 1e-6 && cum[i] < arc_end - 1e-6 {
            pts.push(geometry[i]);
        }
    }

    // Interpolated end point (if different from start).
    let ep = interp_at_arc(geometry, cum, arc_end);
    if pts.last().map(|&p| p != ep).unwrap_or(true) {
        pts.push(ep);
    }

    pts
}

// ── Hex ID parsing ────────────────────────────────────────────────────────────

/// Parse a 32-char hex string (with or without hyphens) into a 16-byte binary key.
/// Used for source formats that identify segments and connectors with hex-encoded
/// 128-bit integers (e.g. UUID-format IDs).
pub fn parse_hex_id(s: &str) -> Result<[u8; 16]> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let bytes = hex::decode(&clean)
        .map_err(|e| anyhow::anyhow!("hex id decode '{}': {}", s, e))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("hex id wrong length (expected 32 hex chars): '{}'", s))
}


// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapt::AdaptedSegment;
    use crate::extract::ConnectorRef;
    use openlr_graph::Direction;

    fn connector(id: &str, at: f64) -> ConnectorRef {
        ConnectorRef { connector_id: id.to_string(), at }
    }

    // Valid 32-hex-char IDs for use in tests.
    const SEG_ID:   &str = "00000000000000000000000000000001";
    const START_ID: &str = "00000000000000000000000000000002";
    const MID_ID:   &str = "00000000000000000000000000000003";
    const END_ID:   &str = "00000000000000000000000000000004";

    fn bare_segment(connectors: Vec<ConnectorRef>) -> AdaptedSegment {
        AdaptedSegment {
            stable_id: SEG_ID.to_string(),
            geometry: vec![(0.0, 0.0), (0.5, 0.0), (1.0, 0.0)],
            connectors,
            frc: 3,
            fow: 3,
            direction: Direction::Both,
            vehicular: true,
            prohibited_transitions: vec![],
        }
    }

    #[test]
    fn single_edge_no_interior_split() {
        let seg = bare_segment(vec![connector(START_ID, 0.0), connector(END_ID, 1.0)]);
        let mut vehicular = HashSet::new();
        vehicular.insert(START_ID.to_string());
        vehicular.insert(END_ID.to_string());
        let (edges, nodes) = split_segment(seg, &vehicular);
        assert_eq!(edges.len(), 1);
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn interior_connector_splits_into_two_edges() {
        let seg = bare_segment(vec![
            connector(START_ID, 0.0),
            connector(MID_ID,   0.5),
            connector(END_ID,   1.0),
        ]);
        let mut vehicular = HashSet::new();
        vehicular.insert(START_ID.to_string());
        vehicular.insert(MID_ID.to_string());
        vehicular.insert(END_ID.to_string());
        let (edges, _) = split_segment(seg, &vehicular);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].split_idx, 0);
        assert_eq!(edges[1].split_idx, 1);
    }

    #[test]
    fn non_vehicular_interior_connector_not_split() {
        let seg = bare_segment(vec![
            connector(START_ID, 0.0),
            connector(MID_ID,   0.5),
            connector(END_ID,   1.0),
        ]);
        // MID_ID is NOT in vehicular_endpoints → no split there.
        let mut vehicular = HashSet::new();
        vehicular.insert(START_ID.to_string());
        vehicular.insert(END_ID.to_string());
        let (edges, _) = split_segment(seg, &vehicular);
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn parse_hex_id_roundtrip() {
        let s = "550e8400e29b41d4a716446655440000";
        let bytes = parse_hex_id(s).unwrap();
        assert_eq!(hex::encode(bytes), s);
    }

    #[test]
    fn parse_hex_id_with_hyphens() {
        let s = "550e8400-e29b-41d4-a716-446655440000";
        let bytes = parse_hex_id(s).unwrap();
        assert_eq!(hex::encode(bytes), "550e8400e29b41d4a716446655440000");
    }
}
