//! FRC/FOW/BEAR/DNP/LFRCNP computation per LRP (whitepaper §11.1 Step 7).
//! All values here are exact (not yet quantized) — the physical-format
//! serializers in `openlr_codec::encoder` apply their own sector/bucket
//! quantization when writing bytes.

use openlr_graph::{bearing_away_from_node, point_and_bearing_into_segment, Graph, NodeId, SegmentId};

use crate::virtual_split::LrpAnchor;

/// Attributes describing one leg (from an LRP's node to the next LRP's node).
pub struct LegAttributes {
    /// FRC of the leg's first segment — the LRP's own "outgoing line" (or, for
    /// the last LRP, the incoming line).
    pub frc: u8,
    pub fow: u8,
    /// Lowest FRC (i.e. numerically highest value) anywhere along the leg.
    pub lfrcnp: u8,
    /// Sum of the leg's segment lengths, meters.
    pub dnp_m: f64,
    /// BEAR: bearing at `node`, degrees. See `bearing_away_from_node` for why
    /// the same formula is correct for both normal and last LRPs.
    pub bearing_deg: f64,
}

/// Compute the FRC/FOW/LFRCNP/DNP/BEAR for the leg starting at `node` and
/// spanning `leg_segments` (in travel order). Returns `None` if any segment is
/// missing from `graph` or the leg is empty.
pub fn leg_attributes(graph: &Graph, node: NodeId, leg_segments: &[SegmentId]) -> Option<LegAttributes> {
    let first_id = *leg_segments.first()?;
    let first_seg = graph.segments.get(&first_id)?;

    let mut lfrcnp = 0u8;
    let mut dnp_m = 0.0f64;
    for seg_id in leg_segments {
        let seg = graph.segments.get(seg_id)?;
        lfrcnp = lfrcnp.max(seg.frc);
        dnp_m += seg.length_m;
    }

    let bearing_deg = bearing_away_from_node(first_seg, node)?;

    Some(LegAttributes {
        frc: first_seg.frc,
        fow: first_seg.fow,
        lfrcnp,
        dnp_m,
        bearing_deg,
    })
}

/// Coordinate + FRC/FOW/LFRCNP/BEAR for a leg anchored by `start` — a real
/// node, or (Rule-1 virtual-point splitting, `crate::virtual_split`) a point
/// partway into `leg_segments[0]`. Generalizes `leg_attributes` to also
/// handle `LrpAnchor::Virtual`; callers use the leg's own precomputed
/// `length_m` for DNP, not this function's `dnp_m` (which double-counts a
/// partially-covered boundary segment whenever `start` is `Virtual`).
pub fn leg_attributes_from_anchor(graph: &Graph, start: &LrpAnchor, leg_segments: &[SegmentId]) -> Option<((f64, f64), LegAttributes)> {
    let first_id = *leg_segments.first()?;
    let first_seg = graph.segments.get(&first_id)?;

    let mut lfrcnp = 0u8;
    let mut dnp_m = 0.0f64;
    for seg_id in leg_segments {
        let seg = graph.segments.get(seg_id)?;
        lfrcnp = lfrcnp.max(seg.frc);
        dnp_m += seg.length_m;
    }

    let (coord, bearing_deg) = match *start {
        LrpAnchor::Node(n) => (graph.nodes.get(&n).map(|nd| (nd.lon, nd.lat))?, bearing_away_from_node(first_seg, n)?),
        LrpAnchor::Virtual { entry_node, dist_from_entry_m, .. } => {
            point_and_bearing_into_segment(first_seg, entry_node, dist_from_entry_m, true)?
        }
    };

    Some((coord, LegAttributes { frc: first_seg.frc, fow: first_seg.fow, lfrcnp, dnp_m, bearing_deg }))
}

/// BEAR only, for the last LRP of a location — it has no leg of its own (DNP
/// and LFRCNP don't apply), just a bearing computed by "looking back" into the
/// final segment of the path, away from the arrival node.
pub fn last_lrp_bearing_deg(graph: &Graph, node: NodeId, last_seg: SegmentId) -> Option<f64> {
    let seg = graph.segments.get(&last_seg)?;
    bearing_away_from_node(seg, node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, NetworkSegment};

    fn seg(id: u32, s: u32, e: u32, frc: u8, len: f64, geom: Vec<(f64, f64)>) -> NetworkSegment {
        NetworkSegment {
            id: SegmentId(id),
            start_node: NodeId(s),
            end_node: NodeId(e),
            geometry: geom,
            length_m: len,
            frc, fow: 2,
            direction: Direction::Both,
            stable_id: String::new(),
        }
    }

    #[test]
    fn aggregates_lfrcnp_and_dnp_over_the_leg() {
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 3, 100.0, vec![(0.0, 0.0), (0.001, 0.0)]));
        g.add_segment(seg(2, 1, 2, 5, 200.0, vec![(0.001, 0.0), (0.002, 0.0)])); // higher (worse) FRC
        let attrs = leg_attributes(&g, NodeId(0), &[SegmentId(1), SegmentId(2)]).unwrap();
        assert_eq!(attrs.frc, 3, "FRC is the leg's first segment, not the max");
        assert_eq!(attrs.lfrcnp, 5, "LFRCNP is the worst FRC anywhere in the leg");
        assert!((attrs.dnp_m - 300.0).abs() < 1e-9);
    }

    #[test]
    fn bearing_faces_forward_along_the_first_segment() {
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 3, 100.0, vec![(0.0, 0.0), (0.01, 0.0)])); // due east
        let attrs = leg_attributes(&g, NodeId(0), &[SegmentId(1)]).unwrap();
        assert!((attrs.bearing_deg - 90.0).abs() < 1.0, "bearing={}", attrs.bearing_deg);
    }

    #[test]
    fn last_lrp_bearing_looks_back_into_the_incoming_segment() {
        let mut g = Graph::new();
        // Segment runs due east (0,0)->(0.01,0); arriving at node 1 (its end).
        g.add_segment(seg(1, 0, 1, 3, 100.0, vec![(0.0, 0.0), (0.01, 0.0)]));
        let bearing = last_lrp_bearing_deg(&g, NodeId(1), SegmentId(1)).unwrap();
        // Looking back (west) from the arrival point, not forward (east).
        assert!((bearing - 270.0).abs() < 1.0, "bearing={bearing}");
    }
}
