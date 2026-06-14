use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rayon::prelude::*;
use tracing::{trace, warn};

use crate::adapt::AdaptedSegment;
use openlr_graph::Direction;

// ── Output types ──────────────────────────────────────────────────────────────

/// One node-to-node directed edge, produced by splitting an Overture segment at
/// every interior connector position (Invariant 1).
#[derive(Debug, Clone)]
pub struct SplitEdge {
    pub start_node_gers: [u8; 16],
    pub end_node_gers: [u8; 16],
    pub geometry: Vec<(f64, f64)>, // (lon, lat), first = start node, last = end node
    pub length_m: f64,
    pub frc: u8,
    pub fow: u8,
    pub direction: Direction,
    /// The parent Overture segment GERS id, used for turn-restriction cross-references.
    pub parent_gers_id: [u8; 16],
}

/// One road node, de-duplicated by GERS id across all edges.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub gers_id: [u8; 16],
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
    // Deduplicate nodes by GERS id; last writer wins (coordinates should agree).
    let mut node_map: HashMap<[u8; 16], NodeRecord> = HashMap::new();

    for (seg_edges, seg_nodes) in results {
        edges.extend(seg_edges);
        for n in seg_nodes {
            node_map.insert(n.gers_id, n);
        }
    }

    let nodes: Vec<NodeRecord> = node_map.into_values().collect();
    (edges, nodes)
}

// ── Per-segment splitting ─────────────────────────────────────────────────────

fn split_segment(seg: AdaptedSegment, vehicular_endpoints: &HashSet<String>) -> (Vec<SplitEdge>, Vec<NodeRecord>) {
    if seg.geometry.len() < 2 {
        warn!(id = %seg.gers_id, "segment has fewer than 2 geometry points, skipped");
        return (vec![], vec![]);
    }

    // Parse the parent GERS id once; skip the whole segment if it's malformed.
    let parent_gers_id = match parse_gers_id(&seg.gers_id) {
        Ok(id) => id,
        Err(e) => {
            warn!(id = %seg.gers_id, error = %e, "segment has invalid GERS id, skipped");
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
        warn!(id = %seg.gers_id, "segment has no connectors, skipped");
        return (vec![], vec![]);
    }

    // Clamp connectors to [0, 1] and ensure we have endpoints.
    let has_start = connectors.first().map(|c| c.at <= 1e-9).unwrap_or(false);
    let has_end   = connectors.last().map(|c| c.at >= 1.0 - 1e-9).unwrap_or(false);

    if !has_start || !has_end {
        warn!(
            id = %seg.gers_id,
            has_start,
            has_end,
            "segment missing endpoint connector, skipped"
        );
        return (vec![], vec![]);
    }

    let cum = cumulative_lengths(&seg.geometry);

    let mut edges = Vec::new();
    let mut node_records: Vec<NodeRecord> = Vec::new();

    for pair in connectors.windows(2) {
        let c_start = &pair[0];
        let c_end   = &pair[1];

        // Skip zero-length spans (duplicate at values).
        if (c_end.at - c_start.at).abs() < 1e-9 {
            continue;
        }

        // Skip edges with malformed connector IDs rather than colliding on zero.
        let start_gers = match parse_gers_id(&c_start.connector_id) {
            Ok(id) => id,
            Err(e) => {
                warn!(id = %seg.gers_id, connector = %c_start.connector_id,
                      error = %e, "invalid start connector GERS id, edge skipped");
                continue;
            }
        };
        let end_gers = match parse_gers_id(&c_end.connector_id) {
            Ok(id) => id,
            Err(e) => {
                warn!(id = %seg.gers_id, connector = %c_end.connector_id,
                      error = %e, "invalid end connector GERS id, edge skipped");
                continue;
            }
        };

        let sub_geom = sub_geometry(&seg.geometry, &cum, c_start.at, c_end.at);
        if sub_geom.len() < 2 {
            warn!(id = %seg.gers_id, at_start = c_start.at, at_end = c_end.at,
                  "sub-geometry degenerate, skipped");
            continue;
        }

        let length_m = polyline_length_m(&sub_geom);
        trace!(id = %seg.gers_id, at_start = c_start.at, at_end = c_end.at,
               length_m, "split edge");

        node_records.push(NodeRecord {
            gers_id: start_gers,
            lon: sub_geom[0].0,
            lat: sub_geom[0].1,
        });
        node_records.push(NodeRecord {
            gers_id: end_gers,
            lon: sub_geom.last().unwrap().0,
            lat: sub_geom.last().unwrap().1,
        });

        edges.push(SplitEdge {
            start_node_gers: start_gers,
            end_node_gers:   end_gers,
            geometry:        sub_geom,
            length_m,
            frc:             seg.frc,
            fow:             seg.fow,
            direction:       seg.direction,
            parent_gers_id,
        });
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

// ── GERS ID parsing ───────────────────────────────────────────────────────────

/// Parse an Overture GERS id (32-char hex string, with or without hyphens) to 16 bytes.
pub fn parse_gers_id(s: &str) -> Result<[u8; 16]> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let bytes = hex::decode(&clean)
        .map_err(|e| anyhow::anyhow!("GERS id hex decode '{}': {}", s, e))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("GERS id wrong length (expected 32 hex chars): '{}'", s))
}


// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapt::AdaptedSegment;
    use crate::extract::ConnectorRef;
    use openlr_graph::Direction;

    fn gers(s: &str) -> String {
        s.to_string()
    }

    fn simple_segment(connectors: Vec<ConnectorRef>) -> AdaptedSegment {
        AdaptedSegment {
            gers_id: "0" .repeat(32),
            geometry: vec![(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)],
            connectors,
            frc: 3,
            fow: 3,
            direction: Direction::Both,
            vehicular: true,
            prohibited_transitions: vec![],
        }
    }

    fn connector(id: &str, at: f64) -> ConnectorRef {
        ConnectorRef { connector_id: id.to_string(), at }
    }

    // 32 zero hex chars = valid GERS id
    const ZERO_GERS: &str = "00000000000000000000000000000000";
    const ONE_GERS:  &str = "00000000000000000000000000000001";
    const TWO_GERS:  &str = "00000000000000000000000000000002";

    fn vehicular_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_edge_no_interior_connectors() {
        let seg = simple_segment(vec![
            connector(ZERO_GERS, 0.0),
            connector(ONE_GERS,  1.0),
        ]);
        let (edges, nodes) = split(vec![seg], &vehicular_set(&[]));
        assert_eq!(edges.len(), 1);
        assert_eq!(nodes.len(), 2); // start + end
        assert!(edges[0].length_m > 0.0);
        assert_eq!(edges[0].geometry.len(), 3); // all original points
    }

    #[test]
    fn interior_connector_produces_two_edges() {
        // ONE_GERS is the interior connector; it must be in the vehicular set to trigger a split.
        let seg = simple_segment(vec![
            connector(ZERO_GERS, 0.0),
            connector(ONE_GERS,  0.5),
            connector(TWO_GERS,  1.0),
        ]);
        let (edges, nodes) = split(vec![seg], &vehicular_set(&[ONE_GERS]));
        assert_eq!(edges.len(), 2);
        assert_eq!(nodes.len(), 3);
        // Both sub-edges should have positive length.
        assert!(edges[0].length_m > 0.0);
        assert!(edges[1].length_m > 0.0);
    }

    #[test]
    fn non_vehicular_interior_connector_not_split() {
        // Interior connector not in the vehicular set → no split.
        let seg = simple_segment(vec![
            connector(ZERO_GERS, 0.0),
            connector(ONE_GERS,  0.5),
            connector(TWO_GERS,  1.0),
        ]);
        let (edges, nodes) = split(vec![seg], &vehicular_set(&[]));
        assert_eq!(edges.len(), 1, "should not split at non-vehicular interior connector");
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn split_preserves_total_length() {
        let seg = AdaptedSegment {
            gers_id: ZERO_GERS.to_string(),
            geometry: vec![(174.0, -36.0), (174.5, -36.0), (175.0, -36.0)],
            connectors: vec![
                connector(ZERO_GERS, 0.0),
                connector(ONE_GERS,  0.5),
                connector(TWO_GERS,  1.0),
            ],
            frc: 2,
            fow: 3,
            direction: Direction::Both,
            vehicular: true,
            prohibited_transitions: vec![],
        };
        let full_len = polyline_length_m(&[(174.0, -36.0), (174.5, -36.0), (175.0, -36.0)]);
        let (edges, _) = split(vec![seg], &vehicular_set(&[ONE_GERS]));
        let total: f64 = edges.iter().map(|e| e.length_m).sum();
        assert!((total - full_len).abs() < 0.1, "total {total} ≈ full {full_len}");
    }

    #[test]
    fn haversine_zero_for_same_point() {
        assert!(haversine_m(174.0, -36.0, 174.0, -36.0) < 1e-9);
    }

    #[test]
    fn haversine_roughly_right() {
        // Roughly 1 degree of longitude at equator ≈ 111_319 m.
        let d = haversine_m(0.0, 0.0, 1.0, 0.0);
        assert!((d - 111_319.0).abs() < 200.0, "got {d}");
    }

    #[test]
    fn parse_gers_id_roundtrip() {
        let s = "08b2a5ca8e3cffff0400344900000001";
        let bytes = parse_gers_id(s).unwrap();
        let back = hex::encode(bytes);
        assert_eq!(back, s);
    }

    #[test]
    fn parse_gers_id_with_hyphens() {
        // UUID-formatted GERS ids
        let s = "08b2a5ca-8e3c-ffff-0400-344900000001";
        let bytes = parse_gers_id(s).unwrap();
        assert_eq!(hex::encode(bytes), "08b2a5ca8e3cffff0400344900000001");
    }

    #[test]
    fn sub_geometry_returns_full_geometry_for_zero_to_one() {
        let geom = vec![(0.0f64, 0.0), (1.0, 0.0), (2.0, 0.0)];
        let cum  = cumulative_lengths(&geom);
        let sub  = sub_geometry(&geom, &cum, 0.0, 1.0);
        assert_eq!(sub.len(), 3);
        assert!((sub[0].0 - 0.0).abs() < 1e-9);
        assert!((sub[2].0 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn sub_geometry_midpoint_split() {
        let geom = vec![(0.0f64, 0.0), (2.0, 0.0)];
        let cum  = cumulative_lengths(&geom);
        let first  = sub_geometry(&geom, &cum, 0.0, 0.5);
        let second = sub_geometry(&geom, &cum, 0.5, 1.0);
        // Each half ends/starts at midpoint (1.0, 0.0)
        assert!((first.last().unwrap().0  - 1.0).abs() < 1e-6);
        assert!((second.first().unwrap().0 - 1.0).abs() < 1e-6);
    }
}
