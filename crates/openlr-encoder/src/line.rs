//! Line location encoding: the full Table 54 pipeline — expand to valid
//! nodes, sweep coverage into legs, compute attributes, assemble offsets.

use openlr_codec::{CircularInterval, LinearInterval, LocationReference, Lrp};
use openlr_graph::{Graph, NodeId, SegmentId, NO_PRIOR_SEG};

use crate::{attributes, coverage, expansion, EncodeError};

/// Rule-1: maximum distance between two consecutive LRPs, meters.
pub const MAX_LEG_M: f64 = 15_000.0;

/// A concrete path on the road network — e.g. Layer 1's waypoint-routing
/// output — plus where within the first/last segment the user's *true*
/// intended start/end point falls (it may be mid-segment, not at a node).
pub struct LineLocationInput {
    pub path: Vec<SegmentId>,
    /// The node `path[0]` is entered from (disambiguates travel direction —
    /// a bare segment list alone doesn't say which end is "first").
    pub start_node: NodeId,
    /// Distance from `start_node` to the true intended start, meters.
    pub start_offset_m: f64,
    /// Distance from the true intended end to the path's exit node, meters.
    pub end_offset_m: f64,
    /// Segment-count boundaries (strictly increasing, each in `1..path.len()`)
    /// marking where each user-drawn waypoint-to-waypoint leg ends within
    /// `path`. Empty for a simple two-waypoint route (a single leg).
    ///
    /// Required so a user-forced via-point that isn't on the true shortest
    /// path between the location's overall start and end doesn't get
    /// silently routed around: the coverage sweep has no notion of
    /// "waypoints" and searches straight to the fixed final end every
    /// round, so a shorter route bypassing the via-point entirely can win
    /// outright — with zero agreement against the drawn path from the very
    /// first segment, leaving nowhere valid to insert an intermediate LRP
    /// and failing with `NoRoute` even though a perfectly good route (via
    /// the point the user actually drew) exists. Sweeping each waypoint leg
    /// independently keeps every sweep's target the next waypoint Layer 1
    /// already reached via a genuine shortest-path search, so re-deriving it
    /// can never disagree (Dijkstra's optimal-substructure property) — this
    /// also always places a real LRP at each via-point, which is required
    /// for the decoder to reproduce the same non-shortcut route.
    pub via_split_points: Vec<usize>,
}

/// `max_turn_deviation_deg` is the same turn-angle cap decode-side A* uses —
/// see `coverage::sweep_coverage`'s doc comment for why the encoder needs it
/// too (its name is decode-only by historical accident, not by scope).
pub fn encode_line(graph: &Graph, input: &LineLocationInput, max_turn_deviation_deg: f64, zoom: u8) -> Result<LocationReference, EncodeError> {
    if input.path.is_empty() {
        return Err(EncodeError::EmptyPath);
    }
    let first_seg_id = input.path[0];
    let last_seg_id = *input.path.last().unwrap();

    let end_node = coverage::trace_end_node(graph, input.start_node, &input.path)
        .ok_or(EncodeError::Disconnected { index: 0 })?;

    // Step 2: expand both ends outward to valid nodes (Rule-4), tracking the
    // segments walked so they can be spliced into the full path.
    let start_exp = expansion::expand_to_valid_node(graph, input.start_node, first_seg_id, MAX_LEG_M);
    let end_exp = expansion::expand_to_valid_node(graph, end_node, last_seg_id, MAX_LEG_M);

    let mut full_path = Vec::with_capacity(start_exp.segments.len() + input.path.len() + end_exp.segments.len());
    full_path.extend(start_exp.segments.iter().rev().copied());
    full_path.extend(input.path.iter().copied());
    full_path.extend(end_exp.segments.iter().copied());

    let expanded_start_node = start_exp.node;
    let expanded_end_node = end_exp.node;

    // Figure 27: final offset = original within-leg offset + expansion distance.
    let pos_offset_m = input.start_offset_m + start_exp.distance_m;
    let neg_offset_m = input.end_offset_m + end_exp.distance_m;

    // Steps 3-6: sweep the expanded path into legs, one waypoint-leg chunk at
    // a time (see `via_split_points`'s doc comment for why this can't just be
    // one sweep over the whole path). Expansion only ever prepends/appends
    // segments, so the caller's split points shift by the prepended count.
    let prefix_len = start_exp.segments.len();
    let mut boundaries: Vec<usize> = input.via_split_points.iter().map(|&p| p + prefix_len).collect();
    boundaries.push(full_path.len());

    let mut legs = Vec::new();
    let mut chunk_start_node = expanded_start_node;
    // The segment leading into `chunk_start_node`, for turn-angle purposes —
    // `NO_PRIOR_SEG` for the very first chunk (the location's own overall
    // start, with no "before" to compare against), then whichever segment
    // the *previous* chunk's last leg actually ended on. Without this, each
    // chunk's own `sweep_coverage` call would start fresh with no incoming-
    // segment bias, silently admitting an arbitrarily sharp turn right at
    // the via-point boundary between two waypoint legs.
    let mut chunk_start_seg = NO_PRIOR_SEG;
    let mut chunk_start_idx = 0usize;
    for boundary in boundaries {
        let chunk = &full_path[chunk_start_idx..boundary];
        let chunk_end_node = if boundary == full_path.len() {
            expanded_end_node
        } else {
            coverage::trace_end_node(graph, chunk_start_node, chunk)
                .ok_or(EncodeError::Disconnected { index: chunk_start_idx })?
        };
        let mut chunk_legs = coverage::sweep_coverage(graph, chunk, chunk_start_node, chunk_start_seg, chunk_end_node, MAX_LEG_M, max_turn_deviation_deg, zoom)?;
        chunk_start_seg = *chunk_legs.last().and_then(|l| l.segments.last()).unwrap_or(&chunk_start_seg);
        legs.append(&mut chunk_legs);
        chunk_start_node = chunk_end_node;
        chunk_start_idx = boundary;
    }
    if legs.is_empty() {
        return Err(EncodeError::EmptyPath);
    }

    // Step 7: attributes per LRP.
    let mut lrps = Vec::with_capacity(legs.len() + 1);
    for leg in &legs {
        let attrs = attributes::leg_attributes(graph, leg.start_node, &leg.segments)
            .ok_or(EncodeError::UnknownSegment(leg.segments[0]))?;
        lrps.push(Lrp {
            coord: node_coord(graph, leg.start_node)?,
            bearing: CircularInterval::point(attrs.bearing_deg),
            frc: attrs.frc,
            fow: attrs.fow,
            lfrcnp: Some(attrs.lfrcnp),
            dnp: Some(LinearInterval::point(attrs.dnp_m)),
            pos_offset: None,
            neg_offset: None,
            pos_offset_raw: None,
            neg_offset_raw: None,
        });
    }

    let last_leg_seg = *legs.last().unwrap().segments.last().unwrap();
    let last_seg = graph.segments.get(&last_leg_seg).ok_or(EncodeError::UnknownSegment(last_leg_seg))?;
    let last_bearing = attributes::last_lrp_bearing_deg(graph, expanded_end_node, last_leg_seg)
        .ok_or(EncodeError::UnknownSegment(last_leg_seg))?;
    lrps.push(Lrp {
        coord: node_coord(graph, expanded_end_node)?,
        bearing: CircularInterval::point(last_bearing),
        frc: last_seg.frc,
        fow: last_seg.fow,
        lfrcnp: None,
        dnp: None,
        pos_offset: None,
        neg_offset: None,
        pos_offset_raw: None,
        neg_offset_raw: None,
    });

    // Offsets, bounded per Rule-5 (must be strictly less than the bracketing
    // leg). v1 scope: error out rather than the spec's full cascade of
    // dropping the boundary LRP and re-deriving against the next leg.
    let first_leg_m = lrps[0].dnp.unwrap().lb;
    if pos_offset_m > 0.0 {
        if pos_offset_m >= first_leg_m {
            return Err(EncodeError::Codec(openlr_codec::EncodeError::OffsetExceedsLeg {
                offset_m: pos_offset_m, leg_m: first_leg_m,
            }));
        }
        lrps[0].pos_offset = Some(LinearInterval::point(pos_offset_m));
    }
    let last_leg_m = lrps[lrps.len() - 2].dnp.unwrap().lb;
    if neg_offset_m > 0.0 {
        if neg_offset_m >= last_leg_m {
            return Err(EncodeError::Codec(openlr_codec::EncodeError::OffsetExceedsLeg {
                offset_m: neg_offset_m, leg_m: last_leg_m,
            }));
        }
        let n = lrps.len() - 1;
        lrps[n].neg_offset = Some(LinearInterval::point(neg_offset_m));
    }

    Ok(LocationReference::Line { lrps })
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
            length_m: len_deg * 111_000.0, // rough, matches the geometry's own extent closely enough for tests
            frc: 3, fow: 2,
            direction: Direction::Both,
            stable_id: String::new(),
        }
    }

    /// A simple 3-node straight line; both ends are dead ends (degree 1),
    /// already valid per Rule-4 — no expansion, no intermediates.
    #[test]
    fn simple_straight_line_encodes_two_lrps() {
        let mut g = Graph::new();
        for i in 0..=2u32 { g.add_node(node(i, i as f64 * 0.001, 0.0)); }
        g.add_segment(seg(1, 0, 1, 0.001));
        g.add_segment(seg(2, 1, 2, 0.001));
        // Nodes 0 and 2 each touch exactly one segment — dead ends, already
        // valid (Rule-4 only invalidates pass-throughs, not dead ends).

        let input = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input, 180.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 2);
        assert!(lrps[0].dnp.is_some());
        assert!(lrps[1].dnp.is_none());
        assert!(lrps[0].pos_offset.is_none());
        assert!(lrps[1].neg_offset.is_none());
    }

    #[test]
    fn nonzero_offsets_are_carried_through() {
        let mut g = Graph::new();
        for i in 0..=2u32 { g.add_node(node(i, i as f64 * 0.001, 0.0)); }
        g.add_segment(seg(1, 0, 1, 0.001));
        g.add_segment(seg(2, 1, 2, 0.001));

        let input = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 20.0,
            end_offset_m: 15.0,
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input, 180.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();
        assert!((lrps[0].pos_offset.unwrap().lb - 20.0).abs() < 1e-6);
        assert!((lrps[1].neg_offset.unwrap().lb - 15.0).abs() < 1e-6);
    }

    /// End-to-end through openlr_codec: encode, then serialize to both
    /// physical formats, and confirm each round-trips via its own decoder.
    #[test]
    fn encoded_line_round_trips_through_both_physical_formats() {
        let mut g = Graph::new();
        for i in 0..=2u32 { g.add_node(node(i, i as f64 * 0.001, 0.0)); }
        g.add_segment(seg(1, 0, 1, 0.001));
        g.add_segment(seg(2, 1, 2, 0.001));

        let input = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input, 180.0, 12).unwrap();

        let v3 = openlr_codec::encode_v3_base64(&loc).unwrap();
        let redecoded_v3 = openlr_codec::decode_v3_base64(&v3).unwrap();
        assert_eq!(redecoded_v3.lrps().unwrap().len(), 2);

        let tpeg = openlr_codec::encode_tpeg_hex(&loc).unwrap();
        let redecoded_tpeg = openlr_codec::decode_tpeg_hex(&tpeg).unwrap();
        assert_eq!(redecoded_tpeg.lrps().unwrap().len(), 2);
    }

    /// A drawn via-point (0→1→2) that is NOT on the shortest 0→2 route (a
    /// direct 0→2 shortcut exists and is shorter than going via node 1).
    /// Without `via_split_points`, the coverage sweep searches straight to
    /// the fixed final end, takes the shortcut instead, disagrees with the
    /// drawn path from the very first segment, and fails with `NoRoute` even
    /// though the user's intended route is perfectly encodable. Passing the
    /// via-point boundary must make this succeed with the via-point forced
    /// into its own LRP (required for the decoder to reproduce the same
    /// non-shortcut route rather than also taking the shortcut).
    ///
    /// Nodes 0 and 2 each get an extra spur to a dead-end so they're already
    /// valid (3 distinct neighbors) per Rule-4 — otherwise, in this tiny
    /// synthetic graph, *every* node has exactly 2 distinct neighbors and
    /// none can be resolved as a real junction, so `expand_to_valid_node`
    /// walks in circles around the 0-1-2 triangle instead of exercising the
    /// thing this test actually targets. Real road networks essentially
    /// never hit that degenerate case, so it isn't otherwise a concern.
    #[test]
    fn via_point_off_the_shortest_path_does_not_get_shortcut_away() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0,    0.0));
        g.add_node(node(1, 0.001,  0.001)); // via-point, off to the side
        g.add_node(node(2, 0.002,  0.0));
        g.add_node(node(3, -0.001, -0.001)); // dead-end spur off node 0
        g.add_node(node(4, 0.003,  -0.001)); // dead-end spur off node 2
        g.add_segment(seg(1, 0, 1, 0.002)); // 0->1, longer leg
        g.add_segment(seg(2, 1, 2, 0.002)); // 1->2, longer leg
        g.add_segment(seg(3, 0, 2, 0.001)); // 0->2 direct shortcut, much shorter
        g.add_segment(seg(4, 0, 3, 0.001)); // spur: makes node 0 a valid 3-way branch
        g.add_segment(seg(5, 2, 4, 0.001)); // spur: makes node 2 a valid 3-way branch

        // Without via_split_points, the sweep takes the direct shortcut and fails.
        let input_no_via = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };
        assert!(matches!(encode_line(&g, &input_no_via, 180.0, 12), Err(EncodeError::NoRoute)));

        // With the via-point boundary declared, it succeeds and forces an
        // LRP exactly at node 1.
        let input_with_via = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![1],
        };
        let loc = encode_line(&g, &input_with_via, 180.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 3, "start + via + end");
        let via_lrp = &lrps[1];
        assert!((via_lrp.coord.0 - 0.001).abs() < 1e-9 && (via_lrp.coord.1 - 0.001).abs() < 1e-9,
            "middle LRP should sit at the via-point (node 1), not be shortcut away");
    }

    /// A via-point (node 2) whose only way onward is a ~165° turn relative to
    /// how the path arrived — sharp, but not the *exact* segment reversal
    /// `shortest_path`'s unconditional U-turn rule already blocks regardless
    /// of any angle threshold. This specifically exercises `encode_line`'s new
    /// `max_turn_deviation_deg` parameter (threaded through to
    /// `coverage::sweep_coverage`'s `shortest_path` calls): the default-style
    /// 150° cap must reject it as `NoRoute`, while a disabled (180°) cap must
    /// still encode it successfully.
    #[test]
    fn sharp_but_not_identical_boundary_turn_is_gated_by_max_turn_deviation() {
        use openlr_graph::{Direction, NetworkNode, NetworkSegment};

        fn n(id: u32, lon: f64, lat: f64) -> NetworkNode {
            NetworkNode { id: NodeId(id), lon, lat, stable_id: String::new(), is_boundary: false }
        }
        fn s(id: u32, start: u32, end: u32, geom: Vec<(f64, f64)>) -> NetworkSegment {
            NetworkSegment {
                id: SegmentId(id), start_node: NodeId(start), end_node: NodeId(end),
                geometry: geom, length_m: 300.0, frc: 3, fow: 3,
                direction: Direction::Both, stable_id: String::new(),
            }
        }

        let mut g = Graph::new();
        g.add_node(n(1, 0.000, 0.000));       // W1 (dead end, degree 1)
        g.add_node(n(2, 0.003, 0.000));       // via-point — seg1 arrives heading east (bearing 90°)
        g.add_node(n(3, 0.0001022, -0.0007765)); // only reachable via seg2, a ~165° turn from seg1
        // seg1: W1 -> via, heading due east (bearing away-from-via, backward, is 270°/west).
        g.add_segment(s(1, 1, 2, vec![(0.000, 0.000), (0.003, 0.000)]));
        // seg2: via -> W3, bearing away-from-via ~255° — only 15° off a dead U-turn (165° deviation).
        g.add_segment(s(2, 2, 3, vec![(0.003, 0.000), (0.0001022, -0.0007765)]));

        let input = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(1),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![1],
        };

        let err = encode_line(&g, &input, 150.0, 12)
            .expect_err("a 165° boundary turn must be rejected under the default-style 150° cap");
        assert!(matches!(err, EncodeError::NoRoute), "err={err:?}");

        let loc = encode_line(&g, &input, 180.0, 12)
            .expect("with the turn-angle gate disabled, the same route should encode successfully");
        assert_eq!(loc.lrps().unwrap().len(), 3, "start + via + end");
    }
}
