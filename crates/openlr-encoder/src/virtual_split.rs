//! Rule-1: OpenLR's own absolute ceiling on the distance between two
//! consecutive LRPs (`line::MAX_LEG_M`, 15km). By the time a leg reaches
//! `split_for_rule1`, `coverage::sweep_coverage`'s own shortcut/divergence-
//! protection logic (`forced_prefix_len`/`best_intermediate_position`/
//! `shortest_path`) has already established that its segment list *is* the
//! correct, shortcut-free path between its two bracketing LRPs. Splitting it
//! further here for length reasons only ever places new LRPs at points
//! along that already-verified path — no new routing decision is being
//! made, so this never re-runs A*/`shortest_path`, unlike
//! `sweep_coverage`'s own divergence-driven splits. (A junction chosen here
//! purely for length could in principle admit a cheaper alternate route via
//! a different incoming segment than the one this leg actually uses — but
//! if so, that surfaces as a loud DNP-validation decode failure, not a
//! silent wrong answer, and is out of scope for this module.)

use openlr_graph::{Graph, NodeId, SegmentId};

use crate::coverage::{other_end, Leg};
use crate::EncodeError;

/// Where a leg boundary sits: exactly on a graph node, or (last resort, only
/// when even a single segment alone exceeds `max_leg_m`) partway into a
/// specific segment — the OpenLR whitepaper's virtual-point splitting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LrpAnchor {
    Node(NodeId),
    Virtual {
        segment: SegmentId,
        /// Whichever of `segment`'s two endpoints this leg's own travel
        /// direction enters it from — see `point_and_bearing_into_segment`.
        entry_node: NodeId,
        /// Distance traveled into `segment` from `entry_node`. Accumulates
        /// by repeatedly adding `max_leg_m` (never derived from
        /// `segment.length_m`), so it stays exactly self-consistent with
        /// `point_and_bearing_into_segment`'s own `polyline_length_m`/
        /// `interpolate_at`-based arc math regardless of any tiny
        /// discrepancy between the stored `length_m` field and the
        /// geometry's true arc length.
        dist_from_entry_m: f64,
    },
}

/// Rule-1: given a leg already known to be a correct, shortcut-free path
/// (`raw_segments`, entered at `raw_start`), subdivide it into one or more
/// legs so no piece exceeds `max_leg_m`. Prefers, in order: the farthest
/// valid (junction) node within budget; if none is in range, the farthest
/// node at all (a pass-through with no real alternative of its own); and
/// only when not even the next segment alone fits in the remaining budget,
/// a virtual point partway into it.
pub fn split_for_rule1(
    graph: &Graph,
    raw_start: NodeId,
    raw_segments: &[SegmentId],
    max_leg_m: f64,
) -> Result<Vec<Leg>, EncodeError> {
    // Guard against a degenerate cap: this walk always makes progress by
    // adding `max_leg_m` per step, which requires it to be a finite positive
    // number or the loop below never terminates.
    if !(max_leg_m > 0.0) || !max_leg_m.is_finite() {
        let length_m: f64 = raw_segments.iter().filter_map(|id| graph.segments.get(id)).map(|s| s.length_m).sum();
        return Err(EncodeError::LegTooLong { length_m, max_leg_m });
    }

    struct Candidate { after_idx: usize, cumulative_m: f64, node: NodeId, is_valid: bool }
    let mut candidates = Vec::with_capacity(raw_segments.len());
    let mut node = raw_start;
    let mut cum = 0.0_f64;
    for (i, seg_id) in raw_segments.iter().enumerate() {
        let seg = graph.segments.get(seg_id).expect("segment in an already-assembled leg must exist");
        cum += seg.length_m;
        node = other_end(graph, *seg_id, node).expect("an already-assembled leg's segments must be contiguous");
        candidates.push(Candidate { after_idx: i, cumulative_m: cum, node, is_valid: graph.is_valid_node(node) });
    }
    let total_m = candidates.last().map(|c| c.cumulative_m).unwrap_or(0.0);

    if total_m <= max_leg_m {
        return Ok(vec![Leg { start: LrpAnchor::Node(raw_start), segments: raw_segments.to_vec(), length_m: total_m }]);
    }

    let mut out = Vec::new();
    let mut cur_start = LrpAnchor::Node(raw_start);
    let mut base_m = 0.0_f64;
    let mut base_idx = 0_usize;

    loop {
        let target_m = base_m + max_leg_m;
        if target_m >= total_m {
            out.push(Leg { start: cur_start, segments: raw_segments[base_idx..].to_vec(), length_m: total_m - base_m });
            break;
        }

        let reachable: Vec<&Candidate> = candidates[base_idx..].iter().take_while(|c| c.cumulative_m <= target_m).collect();

        if let Some(farthest_valid) = reachable.iter().rev().find(|c| c.is_valid) {
            out.push(Leg {
                start: cur_start,
                segments: raw_segments[base_idx..=farthest_valid.after_idx].to_vec(),
                length_m: farthest_valid.cumulative_m - base_m,
            });
            base_m = farthest_valid.cumulative_m;
            base_idx = farthest_valid.after_idx + 1;
            cur_start = LrpAnchor::Node(farthest_valid.node);
            continue;
        }
        if let Some(farthest_any) = reachable.last() {
            out.push(Leg {
                start: cur_start,
                segments: raw_segments[base_idx..=farthest_any.after_idx].to_vec(),
                length_m: farthest_any.cumulative_m - base_m,
            });
            base_m = farthest_any.cumulative_m;
            base_idx = farthest_any.after_idx + 1;
            cur_start = LrpAnchor::Node(farthest_any.node);
            continue;
        }

        // Not even `raw_segments[base_idx]` alone (from wherever `cur_start`
        // currently sits within it) fits in the remaining budget: a virtual
        // cut inside it.
        let seg_id = raw_segments[base_idx];
        let entry_node = match cur_start {
            LrpAnchor::Node(n) => n,
            LrpAnchor::Virtual { entry_node, .. } => entry_node,
        };
        let already_consumed_m = match cur_start {
            LrpAnchor::Node(_) => 0.0,
            LrpAnchor::Virtual { segment, dist_from_entry_m, .. } => {
                debug_assert_eq!(segment, seg_id, "a virtual cur_start must continue into the same segment it cut");
                dist_from_entry_m
            }
        };
        out.push(Leg { start: cur_start, segments: vec![seg_id], length_m: max_leg_m });
        base_m += max_leg_m;
        cur_start = LrpAnchor::Virtual { segment: seg_id, entry_node, dist_from_entry_m: already_consumed_m + max_leg_m };
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, NetworkNode, NetworkSegment};

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: String::new(), is_boundary: false }
    }
    /// A segment running due east from `(s * 0.1, 0.0)`, `len_m` long, tagged
    /// with a geometry whose extent roughly matches (fine-grained enough for
    /// `point_and_bearing_into_segment`'s arc math in these tests).
    fn seg(id: u32, s: u32, e: u32, len_m: f64) -> NetworkSegment {
        let lon0 = s as f64 * 0.1;
        let len_deg = len_m / 111_000.0;
        NetworkSegment {
            id: SegmentId(id),
            start_node: NodeId(s),
            end_node: NodeId(e),
            geometry: vec![(lon0, 0.0), (lon0 + len_deg, 0.0)],
            length_m: len_m,
            frc: 3, fow: 2,
            direction: Direction::Both,
            stable_id: String::new(),
        }
    }

    fn make_valid_junction(g: &mut Graph, at: NodeId, spur_id: u32, spur_to: u32) {
        // A degree-3 node (real branch) is a valid junction per `is_valid_node`.
        g.add_node(node(spur_to, 999.0, 999.0 + spur_to as f64));
        g.add_segment(seg(spur_id, at.0, spur_to, 50.0));
    }

    #[test]
    fn fits_within_budget_returns_single_unsplit_leg() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.1, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));

        let legs = split_for_rule1(&g, NodeId(0), &[SegmentId(1)], 15_000.0).unwrap();
        assert_eq!(legs.len(), 1);
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
        assert_eq!(legs[0].segments, vec![SegmentId(1)]);
        assert!((legs[0].length_m - 100.0).abs() < 1e-6);
    }

    #[test]
    fn multi_segment_leg_prefers_farthest_valid_node_within_budget() {
        // 0 --10km-- 1 --4km-- 2(valid junction) --0.9km-- 3(pass-through) --0.5km-- 4
        // Total = 15.4km > 10km cap. Within a 10km budget from node 0, the
        // farthest node reached is 3 (14.9km... wait -- keep within budget):
        // recompute distances so the farthest-*valid*-within-budget is node 2.
        let mut g = Graph::new();
        for i in 0..=4u32 { g.add_node(node(i, i as f64 * 0.1, 0.0)); }
        g.add_segment(seg(1, 0, 1, 6_000.0));
        g.add_segment(seg(2, 1, 2, 3_000.0)); // cumulative 9km at node 2
        make_valid_junction(&mut g, NodeId(2), 20, 90);
        g.add_segment(seg(3, 2, 3, 900.0));   // cumulative 9.9km at node 3 (pass-through)
        g.add_segment(seg(4, 3, 4, 3_000.0)); // cumulative 12.9km at node 4 (pass-through)

        let path = vec![SegmentId(1), SegmentId(2), SegmentId(3), SegmentId(4)];
        let legs = split_for_rule1(&g, NodeId(0), &path, 10_000.0).unwrap();

        assert_eq!(legs.len(), 2, "12.9km total over a 10km cap needs exactly one split");
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
        assert_eq!(legs[0].segments, vec![SegmentId(1), SegmentId(2)], "should stop at the valid node 2, not push on to invalid nodes 3/4 within budget");
        assert!((legs[0].length_m - 9_000.0).abs() < 1.0, "length_m={}", legs[0].length_m);
        assert!(matches!(legs[1].start, LrpAnchor::Node(NodeId(2))));
        assert_eq!(legs[1].segments, vec![SegmentId(3), SegmentId(4)]);
        assert!((legs[1].length_m - 3_900.0).abs() < 1.0, "length_m={}", legs[1].length_m);
    }

    #[test]
    fn falls_back_to_farthest_invalid_node_when_none_valid_in_range() {
        // 0 --9km-- 1(pass-through) --2km-- 2(pass-through) --2km-- 3(dead end,
        // valid -- but out of reach of the 10km budget regardless).
        let mut g = Graph::new();
        for i in 0..=3u32 { g.add_node(node(i, i as f64 * 0.1, 0.0)); }
        g.add_segment(seg(1, 0, 1, 9_000.0));  // node 1: degree 2 (seg1, seg2) -- pass-through, invalid
        g.add_segment(seg(2, 1, 2, 2_000.0));  // node 2: degree 2 (seg2, seg3) -- pass-through, invalid; cumulative 11km, outside budget
        g.add_segment(seg(3, 2, 3, 2_000.0));  // cumulative 13km

        let path = vec![SegmentId(1), SegmentId(2), SegmentId(3)];
        let legs = split_for_rule1(&g, NodeId(0), &path, 10_000.0).unwrap();

        assert_eq!(legs.len(), 2, "13km total over a 10km cap needs exactly one split");
        assert_eq!(legs[0].segments, vec![SegmentId(1)], "only node 1 (9km) is within the 10km budget -- node 2 (11km) is not");
        assert!((legs[0].length_m - 9_000.0).abs() < 1.0, "length_m={}", legs[0].length_m);
        assert!(matches!(legs[1].start, LrpAnchor::Node(NodeId(1))));
        assert_eq!(legs[1].segments, vec![SegmentId(2), SegmentId(3)]);
        assert!((legs[1].length_m - 4_000.0).abs() < 1.0, "length_m={}", legs[1].length_m);
    }

    #[test]
    fn single_oversized_segment_gets_virtual_cuts() {
        // One 40km segment, no interior node at all.
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 40_000.0 / 111_000.0, 0.0));
        g.add_segment(seg(1, 0, 1, 40_000.0));

        let legs = split_for_rule1(&g, NodeId(0), &[SegmentId(1)], 15_000.0).unwrap();

        assert_eq!(legs.len(), 3, "40km / 15km needs 2 splits -> 3 legs (15+15+10)");
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
        assert!((legs[0].length_m - 15_000.0).abs() < 1e-6);
        match legs[1].start {
            LrpAnchor::Virtual { segment, entry_node, dist_from_entry_m } => {
                assert_eq!(segment, SegmentId(1));
                assert_eq!(entry_node, NodeId(0));
                assert!((dist_from_entry_m - 15_000.0).abs() < 1e-6);
            }
            other => panic!("expected a virtual anchor, got {other:?}"),
        }
        assert!((legs[1].length_m - 15_000.0).abs() < 1e-6);
        match legs[2].start {
            LrpAnchor::Virtual { dist_from_entry_m, .. } => assert!((dist_from_entry_m - 30_000.0).abs() < 1e-6),
            other => panic!("expected a virtual anchor, got {other:?}"),
        }
        assert!((legs[2].length_m - 10_000.0).abs() < 1e-6);
        for seg_id in legs.iter().map(|l| &l.segments) {
            assert_eq!(seg_id, &vec![SegmentId(1)]);
        }
    }

    #[test]
    fn mixed_leg_ends_with_a_virtual_cut_on_the_final_oversized_segment() {
        // 0 --6km-- 1(pass-through) --3km-- 2(pass-through) --20km(oversized,
        // no interior node)-- 3(dead end, valid, but far out of reach).
        // Total 29km over a 15km cap: leg 0 combines seg1+seg2 (9km, both
        // nodes invalid, neither is a preferred stopping point) then a
        // virtual cut partway into the oversized seg3; leg 1 starts at that
        // virtual point and itself needs another virtual cut to finish seg3.
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 6_000.0 / 111_000.0, 0.0));
        g.add_node(node(2, 9_000.0 / 111_000.0, 0.0));
        g.add_node(node(3, 29_000.0 / 111_000.0, 0.0));
        g.add_segment(seg(1, 0, 1, 6_000.0));
        g.add_segment(seg(2, 1, 2, 3_000.0));
        g.add_segment(seg(3, 2, 3, 20_000.0));

        let path = vec![SegmentId(1), SegmentId(2), SegmentId(3)];
        let legs = split_for_rule1(&g, NodeId(0), &path, 15_000.0).unwrap();

        assert_eq!(legs.len(), 3);
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
        assert_eq!(legs[0].segments, vec![SegmentId(1), SegmentId(2)], "no node within budget is preferred/required -- take the farthest (node 2 at 9km)");
        assert!((legs[0].length_m - 9_000.0).abs() < 1e-6);

        assert!(matches!(legs[1].start, LrpAnchor::Node(NodeId(2))));
        assert_eq!(legs[1].segments, vec![SegmentId(3)]);
        assert!((legs[1].length_m - 15_000.0).abs() < 1e-6);

        match legs[2].start {
            LrpAnchor::Virtual { segment, entry_node, dist_from_entry_m } => {
                assert_eq!(segment, SegmentId(3));
                assert_eq!(entry_node, NodeId(2));
                assert!((dist_from_entry_m - 15_000.0).abs() < 1e-6);
            }
            other => panic!("expected a virtual anchor, got {other:?}"),
        }
        assert_eq!(legs[2].segments, vec![SegmentId(3)]);
        assert!((legs[2].length_m - 5_000.0).abs() < 1e-6);
    }

    #[test]
    fn degenerate_max_leg_m_returns_leg_too_long_instead_of_looping() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.1, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));

        assert!(matches!(split_for_rule1(&g, NodeId(0), &[SegmentId(1)], 0.0), Err(EncodeError::LegTooLong { .. })));
        assert!(matches!(split_for_rule1(&g, NodeId(0), &[SegmentId(1)], -5.0), Err(EncodeError::LegTooLong { .. })));
        assert!(matches!(split_for_rule1(&g, NodeId(0), &[SegmentId(1)], f64::NAN), Err(EncodeError::LegTooLong { .. })));
    }
}
