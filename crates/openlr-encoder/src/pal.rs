//! PointAlongLine (PAL) encoding: Table 56's 3-step pipeline. Always exactly
//! 2 LRPs bracketing a single line — no coverage sweep, since there's only
//! ever one line to encode.

use openlr_codec::{CircularInterval, LinearInterval, LocationReference, Lrp, Orientation, SideOfRoad};
use openlr_graph::{Graph, NodeId, SegmentId};

use crate::{attributes, expansion, line::MAX_LEG_M, EncodeError};

pub struct PalLocationInput {
    pub line: SegmentId,
    /// The endpoint of `line` on the "first LRP" side.
    pub start_node: NodeId,
    /// Distance from `start_node` to the actual point, meters.
    pub point_offset_m: f64,
    pub orientation: Orientation,
    pub side_of_road: SideOfRoad,
}

/// `max_turn_deviation_deg` is the same cap `line::encode_line` uses — see
/// `expansion::expand_to_valid_node`'s doc comment for why boundary
/// expansion needs it too, even though PAL itself has no A*-routing step.
/// `max_leg_m` is the same encoder-only Rule-1 policy knob `encode_line`
/// takes — see its doc comment — clamped to `MAX_LEG_M`.
pub fn encode_pal(graph: &Graph, input: &PalLocationInput, max_turn_deviation_deg: f64, max_leg_m: f64) -> Result<LocationReference, EncodeError> {
    let max_leg_m = max_leg_m.min(MAX_LEG_M);
    let seg = graph.segments.get(&input.line).ok_or(EncodeError::UnknownSegment(input.line))?;
    let end_node = if seg.start_node == input.start_node {
        seg.end_node
    } else if seg.end_node == input.start_node {
        seg.start_node
    } else {
        return Err(EncodeError::UnknownNode(input.start_node));
    };

    // Table 56 Step 2: expand both ends to valid nodes (Rule-4) — but unlike
    // `line::encode_line`'s boundary legs, PAL has exactly one leg *always*
    // (no via-points can ever split start and end onto different legs), so
    // both expansions compete for the same `max_leg_m` budget. Expanding them
    // independently, each with the *full* cap, can walk right past it in
    // combination — with nothing to catch that until much later, if at all.
    // Sequential budgeting fixes that: the core (un-expandable) line length
    // is spoken for first, start gets whatever's left, and end gets whatever
    // start didn't use. Deterministic, and composes with the turn-angle
    // stopping condition `expand_to_valid_node` already applies.
    //
    // This directly affects `poff_m` below, not just which node the LRP
    // anchors on — POFF *is* `start_exp.distance_m` plus the original
    // within-segment offset, so however far the start expansion actually
    // walks becomes part of the encoded value on the wire.
    let core_m = seg.length_m;
    let start_budget = (max_leg_m - core_m).max(0.0);
    let start_exp = expansion::expand_to_valid_node(graph, input.start_node, input.line, start_budget, max_turn_deviation_deg);
    let end_budget = (max_leg_m - core_m - start_exp.distance_m).max(0.0);
    let end_exp = expansion::expand_to_valid_node(graph, end_node, input.line, end_budget, max_turn_deviation_deg);

    let poff_m = input.point_offset_m + start_exp.distance_m;

    let mut leg_segments = Vec::with_capacity(start_exp.segments.len() + 1 + end_exp.segments.len());
    leg_segments.extend(start_exp.segments.iter().rev().copied());
    leg_segments.push(input.line);
    leg_segments.extend(end_exp.segments.iter().copied());

    let attrs = attributes::leg_attributes(graph, start_exp.node, &leg_segments)
        .ok_or(EncodeError::UnknownSegment(input.line))?;

    // Rule-1, defense in depth: the sequential budgeting above should make
    // this unreachable in practice, but it's a cheap, explicit backstop for
    // the one case it can't fix — `core_m` alone already over `max_leg_m`,
    // in which case both budgets clamp to zero and expansion is a no-op.
    if attrs.dnp_m > max_leg_m {
        return Err(EncodeError::LegTooLong { length_m: attrs.dnp_m, max_leg_m });
    }

    let last_seg_id = end_exp.segments.last().copied().unwrap_or(input.line);
    let last_seg = graph.segments.get(&last_seg_id).ok_or(EncodeError::UnknownSegment(last_seg_id))?;
    let last_bearing = attributes::last_lrp_bearing_deg(graph, end_exp.node, last_seg_id)
        .ok_or(EncodeError::UnknownSegment(last_seg_id))?;

    if poff_m >= attrs.dnp_m {
        return Err(EncodeError::Codec(openlr_codec::EncodeError::OffsetExceedsLeg {
            offset_m: poff_m, leg_m: attrs.dnp_m,
        }));
    }

    let first = Lrp {
        coord: node_coord(graph, start_exp.node)?,
        bearing: CircularInterval::point(attrs.bearing_deg),
        frc: attrs.frc,
        fow: attrs.fow,
        lfrcnp: Some(attrs.lfrcnp),
        dnp: Some(LinearInterval::point(attrs.dnp_m)),
        pos_offset: if poff_m > 0.0 { Some(LinearInterval::point(poff_m)) } else { None },
        neg_offset: None,
        pos_offset_raw: None,
        neg_offset_raw: None,
    };
    let last = Lrp {
        coord: node_coord(graph, end_exp.node)?,
        bearing: CircularInterval::point(last_bearing),
        frc: last_seg.frc,
        fow: last_seg.fow,
        lfrcnp: None,
        dnp: None,
        pos_offset: None,
        neg_offset: None,
        pos_offset_raw: None,
        neg_offset_raw: None,
    };

    Ok(LocationReference::PointAlongLine {
        lrps: vec![first, last],
        orientation: input.orientation,
        side_of_road: input.side_of_road,
    })
}

fn node_coord(graph: &Graph, node: NodeId) -> Result<(f64, f64), EncodeError> {
    graph.nodes.get(&node).map(|n| (n.lon, n.lat)).ok_or(EncodeError::UnknownNode(node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, NetworkNode, NetworkSegment};

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: String::new(), is_boundary: false }
    }
    fn seg(id: u32, s: u32, e: u32, len_deg: f64) -> NetworkSegment {
        let lon0 = s as f64 * 0.001;
        NetworkSegment {
            id: SegmentId(id),
            start_node: NodeId(s),
            end_node: NodeId(e),
            geometry: vec![(lon0, 0.0), (lon0 + len_deg, 0.0)],
            length_m: len_deg * 111_000.0,
            frc: 4, fow: 3,
            direction: Direction::Both,
            stable_id: String::new(),
        }
    }

    #[test]
    fn pal_encodes_exactly_two_lrps_with_positive_offset() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_segment(seg(1, 0, 1, 0.001));
        // Nodes 0 and 1 each touch only this one segment — dead ends, already
        // valid (Rule-4 only invalidates pass-throughs, not dead ends).

        let input = PalLocationInput {
            line: SegmentId(1),
            start_node: NodeId(0),
            point_offset_m: 40.0,
            orientation: Orientation::NoOrientation,
            side_of_road: SideOfRoad::DirectlyOnOrNA,
        };
        let loc = encode_pal(&g, &input, 150.0, 15_000.0).unwrap();
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 2);
        assert!(lrps[0].dnp.is_some());
        assert!(lrps[1].dnp.is_none());
        assert!((lrps[0].pos_offset.unwrap().lb - 40.0).abs() < 1e-6);

        // Round-trips through both physical formats.
        let v3 = openlr_codec::encode_v3_base64(&loc).unwrap();
        let redecoded = openlr_codec::decode_v3_base64(&v3).unwrap();
        assert!(redecoded.is_point_on_line());

        let tpeg = openlr_codec::encode_tpeg_hex(&loc).unwrap();
        let redecoded_tpeg = openlr_codec::decode_tpeg_hex(&tpeg).unwrap();
        assert!(redecoded_tpeg.is_point_on_line());
    }

    /// node 0 --60m-- node 1 (A, pass-through) --100m(core)-- node 2 (B,
    /// pass-through) --60m-- node 3. Nodes 0 and 3 are dead ends (already
    /// valid); 1 and 2 need Rule-4 expansion. PAL always has exactly one
    /// leg, so both expansions compete for the same `max_leg_m` budget.
    #[test]
    fn pal_sequential_budget_prevents_combined_expansion_overrun() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 60.0 / 111_000.0, 0.0));
        g.add_node(node(2, 160.0 / 111_000.0, 0.0));
        g.add_node(node(3, 220.0 / 111_000.0, 0.0));
        g.add_segment(seg(10, 0, 1, 60.0 / 111_000.0));
        g.add_segment(seg(1, 1, 2, 100.0 / 111_000.0)); // the PAL's own line
        g.add_segment(seg(11, 2, 3, 60.0 / 111_000.0));

        let input = PalLocationInput {
            line: SegmentId(1),
            start_node: NodeId(1),
            point_offset_m: 10.0,
            orientation: Orientation::NoOrientation,
            side_of_road: SideOfRoad::DirectlyOnOrNA,
        };

        // Unrestricted: both expansions fully succeed (60 + 100 + 60 = 220m).
        let loc = encode_pal(&g, &input, 180.0, 15_000.0).unwrap();
        let dnp = loc.lrps().unwrap()[0].dnp.unwrap().lb;
        assert!((dnp - 220.0).abs() < 1.0, "dnp={dnp}");

        // 150m: only 50m left after the 100m core. Neither side's 60m hop
        // fits in 50m, so BOTH expansions must stop at the original
        // (invalid) nodes rather than the combined 220m blowing past the cap.
        let loc = encode_pal(&g, &input, 180.0, 150.0).unwrap();
        let dnp = loc.lrps().unwrap()[0].dnp.unwrap().lb;
        assert!((dnp - 100.0).abs() < 1.0, "dnp={dnp}");

        // 175m: start gets first crack at the 75m remaining slack — its 60m
        // hop fits, so start fully expands. End then only has 15m left, and
        // its own 60m hop doesn't fit, so end stays put. Confirms "start
        // gets first dibs" rather than an arbitrary even split.
        let loc = encode_pal(&g, &input, 180.0, 175.0).unwrap();
        let dnp = loc.lrps().unwrap()[0].dnp.unwrap().lb;
        assert!((dnp - 160.0).abs() < 1.0, "dnp={dnp}");

        // 50m: less than the core (100m) alone — both budgets clamp to zero,
        // expansion is a no-op, and the explicit Rule-1 backstop must catch
        // the still-oversized leg rather than silently encoding it.
        match encode_pal(&g, &input, 180.0, 50.0) {
            Err(EncodeError::LegTooLong { length_m, max_leg_m }) => {
                assert!((length_m - 100.0).abs() < 1.0, "length_m={length_m}");
                assert_eq!(max_leg_m, 50.0);
            }
            other => panic!("expected LegTooLong when the 100m core alone exceeds a 50m cap, got {other:?}"),
        }
    }
}
