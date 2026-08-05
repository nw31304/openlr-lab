//! Line location encoding: the full Table 54 pipeline — expand to valid
//! nodes, sweep coverage into legs, compute attributes, assemble offsets.

use openlr_codec::{CircularInterval, LinearInterval, LocationReference, Lrp};
use openlr_graph::{point_and_bearing_into_segment, Graph, NodeId, SegmentId, NO_PRIOR_SEG};

use crate::virtual_split::LrpAnchor;
use crate::{attributes, coverage, expansion, EncodeError};

/// Rule-1: the OpenLR architecture's own absolute ceiling on the distance
/// between two consecutive LRPs, meters — not a default, a hard maximum.
/// A caller's own `max_leg_m` (e.g. a head-unit wanting a smaller tile
/// footprint per leg) is clamped to this; it can only ever be tightened,
/// never loosened, since exceeding it isn't representable in the wire
/// format at all (DNP's byte encoding is a fixed 256-bucket span over
/// exactly this range — see `openlr_codec`'s `DNP_BUCKET_M`).
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
    /// Waypoints only select *which segments* the location covers (Layer 1's
    /// job) — they never dictate where LRPs get placed; `sweep_coverage`
    /// decides that purely from where the drawn path actually diverges from
    /// what a decoder would do. These boundaries are used only to size each
    /// end's Rule-4 expansion budget independently (see the `start_budget`/
    /// `end_budget` computation just below) — a leg's own core length is
    /// bounded by its nearest via-point, not by the whole route.
    pub via_split_points: Vec<usize>,
}

/// `max_turn_deviation_deg` is the same turn-angle cap decode-side A* uses —
/// see `coverage::sweep_coverage`'s doc comment for why the encoder needs it
/// too (its name is decode-only by historical accident, not by scope).
///
/// `max_leg_m` is an encoder-only policy knob — nothing on the decode side
/// depends on it (tile prefetch and A*'s search radius are already sized
/// from the *actual* per-leg DNP the wire data carries, not from any assumed
/// maximum), so a caller can shrink it purely to keep its own encoded
/// references leg-sized for e.g. a constrained decoder's tile budget, with
/// no coordination needed beyond both sides reading the same v3/TPEG bytes.
/// Clamped to `MAX_LEG_M`, the architecture's own absolute ceiling.
pub fn encode_line(graph: &Graph, input: &LineLocationInput, max_turn_deviation_deg: f64, max_leg_m: f64, zoom: u8) -> Result<LocationReference, EncodeError> {
    let max_leg_m = max_leg_m.min(MAX_LEG_M);
    if input.path.is_empty() {
        return Err(EncodeError::EmptyPath);
    }
    let first_seg_id = input.path[0];
    let last_seg_id = *input.path.last().unwrap();

    let end_node = coverage::trace_end_node_validated(graph, input.start_node, &input.path)?;

    // Step 2: expand both ends outward to valid nodes (Rule-4), tracking the
    // segments walked so they can be spliced into the full path.
    //
    // Budget for each expansion depends on whether the start and end
    // boundary share a leg. With via-points splitting the route, the first
    // leg's own core length (before any expansion) only ever competes with
    // the *start* expansion for `max_leg_m`, and the last leg's core only
    // competes with the *end* expansion — independent budgets. With no
    // via-points at all (a single leg spanning the whole location), both
    // expansions compete for the exact same budget, so they're resolved
    // sequentially instead — start first (using the full remaining slack),
    // then end (using whatever start didn't use) — rather than each
    // independently assuming the full cap and silently walking straight
    // past it in combination (see `pal::encode_pal`, which always hits this
    // case since PAL can never have via-points).
    let segs_len_m = |ids: &[SegmentId]| -> f64 {
        ids.iter().filter_map(|id| graph.segments.get(id)).map(|s| s.length_m).sum()
    };
    let first_leg_core_end = input.via_split_points.first().copied().unwrap_or(input.path.len());
    let last_leg_core_start = input.via_split_points.last().copied().unwrap_or(0);
    let first_leg_core_m = segs_len_m(&input.path[..first_leg_core_end]);
    let last_leg_core_m = segs_len_m(&input.path[last_leg_core_start..]);

    let start_budget = (max_leg_m - first_leg_core_m).max(0.0);
    let start_exp = expansion::expand_to_valid_node(graph, input.start_node, first_seg_id, false, start_budget, max_turn_deviation_deg);
    let end_budget = if input.via_split_points.is_empty() {
        // Same leg as the start boundary — only what start didn't use is left.
        (max_leg_m - last_leg_core_m - start_exp.distance_m).max(0.0)
    } else {
        (max_leg_m - last_leg_core_m).max(0.0)
    };
    let end_exp = expansion::expand_to_valid_node(graph, end_node, last_seg_id, true, end_budget, max_turn_deviation_deg);

    let mut full_path = Vec::with_capacity(start_exp.segments.len() + input.path.len() + end_exp.segments.len());
    full_path.extend(start_exp.segments.iter().rev().copied());
    full_path.extend(input.path.iter().copied());
    full_path.extend(end_exp.segments.iter().copied());

    let expanded_start_node = start_exp.node;
    let expanded_end_node = end_exp.node;

    // Figure 27: final offset = original within-leg offset + expansion distance.
    let pos_offset_m = input.start_offset_m + start_exp.distance_m;
    let neg_offset_m = input.end_offset_m + end_exp.distance_m;

    // Steps 3-6: sweep the expanded path into legs in one pass. Every LRP's
    // bearing/FRC/FOW are drawn from the segment it should point the decoder
    // along next, so a leg's own first hop (and any further hop through a
    // pass-through node with no real alternative — which is exactly what
    // Rule-4 expansion only ever walks through) is never a candidate the
    // decoder weighs against some other route; `sweep_coverage` never even
    // runs a comparison search over that ground. An LRP is inserted only
    // where a search from the first genuine decision point actually
    // diverges from the drawn path — never merely because a waypoint (or an
    // expansion seam) happens to sit somewhere.
    let mut legs = coverage::sweep_coverage(
        graph, &full_path, expanded_start_node, NO_PRIOR_SEG, expanded_end_node,
        max_leg_m, max_turn_deviation_deg, zoom,
    )?;
    if legs.is_empty() {
        return Err(EncodeError::EmptyPath);
    }

    // Rule-5 cascade: `pos_offset_m`/`neg_offset_m` include the full Rule-4
    // expansion distance, but `sweep_coverage`'s own divergence protection
    // can split a leg short partway through that expansion (a shortcut found
    // there) -- so the true start/end can legitimately land past the first
    // (or before the last) leg entirely. When that happens, per the
    // whitepaper, drop that boundary leg, reduce the offset by its length,
    // and re-apply against the next leg in. Placing a marker along a path
    // whose correctness `sweep_coverage` already established is not a new
    // routing decision, so (as with Rule-1's `virtual_split`) this never
    // re-runs A*/divergence checks -- it only ever adjusts bookkeeping over
    // legs already known to be correct.
    //
    // `final_anchor` tracks the location's own overall end -- normally the
    // real node `expanded_end_node`, but reassigned to whatever the dropped
    // leg's own start was whenever the neg-offset cascade drops a trailing
    // leg (which may itself be a Rule-1 virtual, mid-segment point).
    let mut final_anchor = LrpAnchor::Node(expanded_end_node);
    let mut pos_offset_m = pos_offset_m;
    let mut neg_offset_m = neg_offset_m;

    while legs.len() > 1 && pos_offset_m >= legs[0].length_m {
        pos_offset_m -= legs[0].length_m;
        legs.remove(0);
    }
    assert!(
        pos_offset_m < legs[0].length_m,
        "Rule-5 cascade exhausted every leg -- pos_offset_m computed upstream (input.start_offset_m + \
         Rule-4 expansion distance) must never reach the total encoded path length; this is an encoder \
         bug (not a caller error), since the expansion distance is itself bounded by the same max_leg_m \
         budget that bounds every leg"
    );

    while legs.len() > 1 && neg_offset_m >= legs.last().unwrap().length_m {
        neg_offset_m -= legs.last().unwrap().length_m;
        let dropped = legs.pop().unwrap();
        final_anchor = dropped.start;
    }
    assert!(
        neg_offset_m < legs.last().unwrap().length_m,
        "Rule-5 cascade exhausted every leg -- neg_offset_m computed upstream (input.end_offset_m + \
         Rule-4 expansion distance) must never reach the total encoded path length; this is an encoder \
         bug (not a caller error), since the expansion distance is itself bounded by the same max_leg_m \
         budget that bounds every leg"
    );

    // Step 7: attributes per LRP. `leg.length_m` (not `attrs.dnp_m`) is the
    // authoritative DNP -- summing segment lengths would double-count a
    // partially-covered boundary segment whenever Rule-1 virtual-point
    // splitting (`virtual_split`) anchored this leg mid-segment.
    let mut lrps = Vec::with_capacity(legs.len() + 1);
    for leg in &legs {
        let (coord, attrs) = attributes::leg_attributes_from_anchor(graph, &leg.start, &leg.segments)
            .ok_or(EncodeError::UnknownSegment(leg.segments[0]))?;
        lrps.push(Lrp {
            coord,
            bearing: CircularInterval::point(attrs.bearing_deg),
            frc: attrs.frc,
            fow: attrs.fow,
            lfrcnp: Some(attrs.lfrcnp),
            dnp: Some(LinearInterval::point(leg.length_m)),
            pos_offset: None,
            neg_offset: None,
            pos_offset_raw: None,
            neg_offset_raw: None,
        });
    }

    // The final LRP has no leg of its own (no DNP/LFRCNP) and its bearing
    // faces backward -- looking back the way arrived, per the whitepaper's
    // reversed final-LRP convention -- rather than forward like every other
    // LRP above. `final_anchor` is normally the real node `expanded_end_node`,
    // but may be a Rule-1 virtual (mid-segment) point if the Rule-5 cascade
    // above dropped a trailing leg that was itself virtually split.
    let last_leg_seg = *legs.last().unwrap().segments.last().unwrap();
    let last_seg = graph.segments.get(&last_leg_seg).ok_or(EncodeError::UnknownSegment(last_leg_seg))?;
    let (final_coord, final_bearing) = match final_anchor {
        LrpAnchor::Node(n) => (
            node_coord(graph, n)?,
            attributes::last_lrp_bearing_deg(graph, n, last_leg_seg).ok_or(EncodeError::UnknownSegment(last_leg_seg))?,
        ),
        LrpAnchor::Virtual { segment, entry_node, dist_from_entry_m } => {
            // `segment` (from a leg's own start anchor) is only ever
            // `Virtual` when it's a continuation within a single
            // `virtual_split::split_for_rule1` call (the *first* leg of any
            // fresh call always starts at a real node) -- so the
            // immediately-preceding leg (now `legs.last()`, since the leg
            // that used to follow it was just popped above) is guaranteed
            // to be from that same call, ending on this same segment.
            debug_assert_eq!(segment, last_leg_seg, "a virtual final_anchor must land on the preceding leg's own last segment");
            point_and_bearing_into_segment(last_seg, entry_node, dist_from_entry_m, false)
                .ok_or(EncodeError::UnknownSegment(last_leg_seg))?
        }
    };
    lrps.push(Lrp {
        coord: final_coord,
        bearing: CircularInterval::point(final_bearing),
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
    // leg) -- guaranteed by the cascade above, so no further check is needed
    // here, just recording the (possibly reduced) values.
    if pos_offset_m > 0.0 {
        lrps[0].pos_offset = Some(LinearInterval::point(pos_offset_m));
    }
    if neg_offset_m > 0.0 {
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
        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 2);
        assert!(lrps[0].dnp.is_some());
        assert!(lrps[1].dnp.is_none());
        assert!(lrps[0].pos_offset.is_none());
        assert!(lrps[1].neg_offset.is_none());
    }

    /// Same straight line as above, but with a via-point declared at node 1
    /// (as if the user had inserted a third waypoint sitting exactly on the
    /// already-drawn route). Since no shortcut around node 1 exists, the
    /// unconstrained sweep reproduces the whole route in one leg — the
    /// via-point should NOT force an intermediate LRP. Waypoints pin down
    /// which segments the location covers, not where LRPs get placed.
    #[test]
    fn via_point_on_the_shortest_path_does_not_force_an_extra_lrp() {
        let mut g = Graph::new();
        for i in 0..=2u32 { g.add_node(node(i, i as f64 * 0.001, 0.0)); }
        g.add_segment(seg(1, 0, 1, 0.001));
        g.add_segment(seg(2, 1, 2, 0.001));

        let input = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![1], // declares a via-point at node 1
        };
        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 2, "no shortcut exists around the via-point, so it should not force an extra LRP");
    }

    /// node 0 --60m-- node 1 (A, pass-through) --100m(core)-- node 2 (B,
    /// pass-through) --60m-- node 3. Nodes 0 and 3 are dead ends (already
    /// valid); 1 and 2 need Rule-4 expansion. With no via-points, start and
    /// end share the location's one and only leg, so both expansions
    /// compete for the same `max_leg_m` budget — same shape as PAL's
    /// `pal_sequential_budget_prevents_combined_expansion_overrun`, since a
    /// via-point-free Line location hits exactly the same "always one
    /// shared leg" case PAL is permanently stuck with.
    #[test]
    fn no_via_points_shares_one_leg_budget_sequentially() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 60.0 / 111_000.0, 0.0));
        g.add_node(node(2, 160.0 / 111_000.0, 0.0));
        g.add_node(node(3, 220.0 / 111_000.0, 0.0));
        g.add_segment(seg(10, 0, 1, 60.0 / 111_000.0));
        g.add_segment(seg(1, 1, 2, 100.0 / 111_000.0)); // the drawn/core path
        g.add_segment(seg(11, 2, 3, 60.0 / 111_000.0));

        let input = LineLocationInput {
            path: vec![SegmentId(1)],
            start_node: NodeId(1),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };

        // Unrestricted: both expansions fully succeed (60 + 100 + 60 = 220m).
        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12).unwrap();
        let dnp = loc.lrps().unwrap()[0].dnp.unwrap().lb;
        assert!((dnp - 220.0).abs() < 1.0, "dnp={dnp}");

        // 150m: only 50m left after the 100m core — neither side's 60m hop
        // fits, so both expansions stay at the original (invalid) nodes.
        let loc = encode_line(&g, &input, 180.0, 150.0, 12).unwrap();
        let dnp = loc.lrps().unwrap()[0].dnp.unwrap().lb;
        assert!((dnp - 100.0).abs() < 1.0, "dnp={dnp}");

        // 175m: start gets first crack at the 75m slack (its 60m hop fits),
        // leaving only 15m for end (whose 60m hop doesn't fit).
        let loc = encode_line(&g, &input, 180.0, 175.0, 12).unwrap();
        let dnp = loc.lrps().unwrap()[0].dnp.unwrap().lb;
        assert!((dnp - 160.0).abs() < 1.0, "dnp={dnp}");

        // 50m: less than the core alone — both budgets clamp to zero, and
        // the 100m core (with no interior node, per node 2's own pass-through
        // status) now gets a Rule-1 virtual-point split instead of erroring:
        // 3 LRPs (node 1, a virtual point 50m in, node 2), each leg exactly 50m.
        let loc = encode_line(&g, &input, 180.0, 50.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 3, "the 100m core needed exactly one virtual-point split");
        assert!((lrps[0].dnp.unwrap().lb - 50.0).abs() < 1.0);
        assert!((lrps[1].dnp.unwrap().lb - 50.0).abs() < 1.0);
        assert!(lrps[2].dnp.is_none(), "the final LRP has no leg of its own");
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
        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12).unwrap();
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
        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12).unwrap();

        let v3 = openlr_codec::encode_v3_base64(&loc).unwrap();
        let redecoded_v3 = openlr_codec::decode_v3_base64(&v3).unwrap();
        assert_eq!(redecoded_v3.lrps().unwrap().len(), 2);

        let tpeg = openlr_codec::encode_tpeg_hex(&loc).unwrap();
        let redecoded_tpeg = openlr_codec::decode_tpeg_hex(&tpeg).unwrap();
        assert_eq!(redecoded_tpeg.lrps().unwrap().len(), 2);
    }

    /// Rule-5 cascade (pos-offset side): node A (dead end, 0,0) --50m-- node X
    /// (pass-through, invalid) --50m(core)-- node B (dead end). The drawn
    /// path starts at X with a 40m within-segment offset; X is invalid, so
    /// Rule-4 expansion walks 50m back to A. `pos_offset_m` = 40 + 50 = 90m,
    /// but a 60m `max_leg_m` forces Rule-1 to split the (A,X,B) 100m path
    /// into two 50m legs at X (the only node between A and B) -- so the
    /// first leg (A to X) is only 50m, shorter than the 90m offset. The true
    /// start lies entirely past it, in the second leg: the cascade must
    /// drop the first leg, reduce the offset to 90-50=40m, and re-apply it
    /// to the (now first) X-to-B leg -- collapsing 3 LRPs down to 2.
    #[test]
    fn rule5_cascade_drops_a_leading_leg_the_offset_entirely_bypasses() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));        // A: dead end
        g.add_node(node(1, 50.0 / 111_000.0, 0.0)); // X: pass-through (invalid)
        g.add_node(node(2, 100.0 / 111_000.0, 0.0)); // B: dead end
        g.add_segment(seg(10, 0, 1, 50.0 / 111_000.0)); // A -> X, 50m
        g.add_segment(seg(1, 1, 2, 50.0 / 111_000.0));  // X -> B, the drawn core, 50m

        let input = LineLocationInput {
            path: vec![SegmentId(1)],
            start_node: NodeId(1), // X: invalid, forces Rule-4 expansion to A
            start_offset_m: 40.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input, 180.0, 60.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();

        assert_eq!(lrps.len(), 2, "the A-X leg should be dropped entirely, collapsing 3 LRPs to 2");
        assert!((lrps[0].coord.0 - (50.0 / 111_000.0)).abs() < 1e-9, "first LRP should now sit at X, not A");
        assert!((lrps[0].dnp.unwrap().lb - 50.0).abs() < 1.0, "dnp={:?}", lrps[0].dnp);
        assert!((lrps[0].pos_offset.unwrap().lb - 40.0).abs() < 1.0, "pos_offset={:?}", lrps[0].pos_offset);
        assert!(lrps[1].dnp.is_none());
    }

    /// Rule-5 cascade (neg-offset side): mirrors the pos-offset test, but
    /// trims from the end. node A (dead end) --50m(core)-- node X
    /// (pass-through, invalid) --50m-- node B (dead end). The drawn path
    /// ends at X with a 40m within-segment end-offset; Rule-4 expansion
    /// walks 50m forward from X to B, so `neg_offset_m` = 40 + 50 = 90m. A
    /// 60m `max_leg_m` again splits the 100m (A,X,B) path at X into two 50m
    /// legs, so the *last* leg (X to B) is only 50m -- shorter than the 90m
    /// offset. The cascade must drop that trailing leg, reduce the offset
    /// to 40m, and re-anchor the final LRP at X -- with a *backward-facing*
    /// bearing (into the A-X segment, looking back from X), not the forward
    /// bearing X's leg-start LRP would otherwise have had.
    #[test]
    fn rule5_cascade_drops_a_trailing_leg_and_recomputes_backward_bearing() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));        // A: dead end
        g.add_node(node(1, 50.0 / 111_000.0, 0.0)); // X: pass-through (invalid)
        g.add_node(node(2, 100.0 / 111_000.0, 0.0)); // B: dead end
        g.add_segment(seg(1, 0, 1, 50.0 / 111_000.0));  // A -> X, the drawn core, 50m
        g.add_segment(seg(11, 1, 2, 50.0 / 111_000.0)); // X -> B, 50m

        let input = LineLocationInput {
            path: vec![SegmentId(1)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 40.0, // true end sits 40m *before* X along the core
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input, 180.0, 60.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();

        assert_eq!(lrps.len(), 2, "the X-B leg should be dropped entirely, collapsing 3 LRPs to 2");
        assert!((lrps[0].dnp.unwrap().lb - 50.0).abs() < 1.0, "dnp={:?}", lrps[0].dnp);
        assert!(lrps[1].dnp.is_none());
        assert!((lrps[1].coord.0 - (50.0 / 111_000.0)).abs() < 1e-9, "final LRP should now sit at X, not B");
        assert!((lrps[1].neg_offset.unwrap().lb - 40.0).abs() < 1.0, "neg_offset={:?}", lrps[1].neg_offset);
        // Backward-facing: looking back (west, ~270°) into the A->X segment
        // from X, not forward (east, ~90°) as X's own leg-start LRP would
        // have faced had it not been converted into the final LRP.
        assert!((lrps[1].bearing.lb_deg - 270.0).abs() < 1.0, "bearing={:?}", lrps[1].bearing);
    }

    /// Same scenario as `rule5_cascade_drops_a_trailing_leg_and_recomputes_backward_bearing`,
    /// but proved end-to-end through the real decode engine rather than just
    /// inspecting the produced LRP fields directly -- the backward-bearing
    /// recompute is new, easy-to-get-subtly-wrong logic, so this is worth
    /// nailing down the same way the Rule-1 virtual-point case was.
    #[test]
    fn rule5_trailing_cascade_round_trips_through_the_real_decode_engine() {
        use openlr_engine::{decode, DecodeParams, Preset};

        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 50.0 / 111_000.0, 0.0));
        g.add_node(node(2, 100.0 / 111_000.0, 0.0));
        g.add_segment(seg(1, 0, 1, 50.0 / 111_000.0));
        g.add_segment(seg(11, 1, 2, 50.0 / 111_000.0));

        let input = LineLocationInput {
            path: vec![SegmentId(1)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 40.0,
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input, 180.0, 60.0, 12).unwrap();

        let v3 = openlr_codec::encode_v3_base64(&loc).unwrap();
        let redecoded = openlr_codec::decode_v3_base64(&v3).unwrap();

        let params = DecodeParams::preset(Preset::Permissive);
        let result = decode(&redecoded, &g, &params, 12).expect("engine decode of a Rule-5 cascaded location failed");
        let decoded = result.as_network().unwrap();
        assert_eq!(decoded.path, vec![SegmentId(1)], "decoder must recover the single A->X segment, not wander onto X->B");
        let neg_offset = decoded.neg_offset.expect("a 40m neg_offset was encoded");
        assert!((neg_offset.lb - 40.0).abs() < 5.0, "neg_offset={neg_offset:?}");
    }

    /// End-to-end proof (not just code-inspection) that the existing decode
    /// engine already handles a Rule-1 virtual-point-split LRP correctly via
    /// its generic per-leg candidate/routing logic (see `virtual_split`'s
    /// module doc) -- no `openlr-engine` changes were made for this feature,
    /// so this is the one assumption most worth nailing down empirically. A
    /// single 40km segment (no interior node at all) needs two virtual-point
    /// splits; the real decode engine must still recover the full path.
    #[test]
    fn virtual_point_split_location_round_trips_through_the_real_decode_engine() {
        use openlr_engine::{decode, DecodeParams, Preset};

        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 40_000.0 / 111_000.0, 0.0));
        g.add_segment(seg(1, 0, 1, 40_000.0 / 111_000.0)); // length_m = len_deg * 111_000.0 = 40_000.0 exactly

        let input = LineLocationInput {
            path: vec![SegmentId(1)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12).unwrap();
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 4, "40km / 15km needs 2 virtual-point splits -> 3 legs -> 4 LRPs");

        let v3 = openlr_codec::encode_v3_base64(&loc).unwrap();
        let redecoded = openlr_codec::decode_v3_base64(&v3).unwrap();

        let params = DecodeParams::preset(Preset::Permissive);
        let result = decode(&redecoded, &g, &params, 12).expect("engine decode of a virtual-point-split location failed");
        let decoded = result.as_network().unwrap();
        let decoded_len_m: f64 = decoded.path.iter().filter_map(|id| g.segments.get(id)).map(|s| s.length_m).sum();
        assert!((decoded_len_m - 40_000.0).abs() < 200.0, "decoded_len_m={decoded_len_m}");
        assert_eq!(decoded.path, vec![SegmentId(1)]);
    }

    /// A drawn via-point (0→1→2) that is NOT on the shortest 0→2 route (a
    /// direct 0→2 shortcut exists and is shorter than going via node 1). The
    /// shortcut leaves from node 0 itself — the very node the first LRP sits
    /// on — so that LRP's own bearing already commits the decoder to seg1,
    /// not the shortcut; node 1 is a plain pass-through (no real
    /// alternative), so the whole route is forced ground and no comparison
    /// search ever runs to "discover" the shortcut in the first place. This
    /// holds regardless of whether a via-point is declared at node 1 —
    /// waypoints pin down which segments the location covers, not where LRPs
    /// get placed, and here nothing forces an LRP there either way.
    ///
    /// Nodes 0 and 2 each get an extra spur to a dead-end so they're already
    /// valid (3 distinct neighbors) per Rule-4 — otherwise, in this tiny
    /// synthetic graph, *every* node has exactly 2 distinct neighbors and
    /// none can be resolved as a real junction, so `expand_to_valid_node`
    /// walks in circles around the 0-1-2 triangle instead of exercising the
    /// thing this test actually targets. Real road networks essentially
    /// never hit that degenerate case, so it isn't otherwise a concern.
    #[test]
    fn via_point_off_the_shortest_path_is_not_shortcut_away_either_way() {
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

        // Without via_split_points: the shortcut leaves from the first LRP's
        // own node, so it's never a candidate to begin with.
        let input_no_via = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };
        let loc = encode_line(&g, &input_no_via, 180.0, 15_000.0, 12).unwrap();
        assert_eq!(loc.lrps().unwrap().len(), 2, "start + end, no via forced");

        // Declaring the via-point boundary doesn't change anything either —
        // it still doesn't force its own LRP, since nothing ever needed
        // protecting from the shortcut.
        let input_with_via = LineLocationInput {
            path: vec![SegmentId(1), SegmentId(2)],
            start_node: NodeId(0),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![1],
        };
        let loc = encode_line(&g, &input_with_via, 180.0, 15_000.0, 12).unwrap();
        assert_eq!(loc.lrps().unwrap().len(), 2, "declaring the via-point doesn't force an extra LRP");
    }

    /// Regression test for a real bug report: a plain two-waypoint route (no
    /// interior via-points at all) whose drawn boundary sits on an invalid
    /// pass-through node. Rule-4 expansion walks it out to a real junction
    /// (node 0) that happens to have a much shorter direct route to the
    /// fixed far end than the user's originally-drawn path. Before the fix,
    /// the coverage sweep ran an unconstrained comparison search from node 0
    /// itself, which naturally preferred that shortcut, disagreed with the
    /// drawn path from the very first segment, and had no via-point to
    /// recover to (via_split_points is necessarily empty for a bare
    /// two-waypoint route) — a hard `NoRoute`, even though the live preview
    /// (a real, connected route) had just shown the user this exact path
    /// existed. The actual fix: node 0's own LRP bearing already commits the
    /// decoder to the drawn segment, not the shortcut, and node 1 (the
    /// original boundary) is a plain pass-through with no alternative of its
    /// own — so the entire route is forced ground, and no comparison search
    /// ever runs to "discover" the shortcut in the first place.
    ///
    /// Graph: node 1 (pass-through boundary) --seg1(core, 100m)-- node 2
    /// (fixed end, already valid). node 1 also connects via seg2 (90m) to
    /// node 0, which is a real 3-way junction (spur to node 5) that also
    /// connects directly to node 2 via seg3 (a 20m shortcut) — a shortcut
    /// that would win handily if a search from node 0 were ever run, which
    /// is exactly why it must not be.
    #[test]
    fn expansion_seam_shortcut_is_never_a_candidate_the_decoder_considers() {
        let mut g = Graph::new();
        g.add_node(node(0, -0.001, 0.0));  // real junction, reached by start-expansion
        g.add_node(node(1, 0.0,    0.0));  // pass-through drawn boundary
        g.add_node(node(2, 0.001,  0.0));  // fixed end, already valid
        g.add_node(node(3, 0.0011, 0.001)); // spur off node 2
        g.add_node(node(4, 0.0011, -0.001)); // spur off node 2
        g.add_node(node(5, -0.002, 0.001)); // spur off node 0
        g.add_segment(seg(1, 1, 2, 100.0 / 111_000.0));  // core: node1 <-> node2, 100m
        g.add_segment(seg(2, 0, 1, 90.0 / 111_000.0));   // node0 <-> node1 (expansion segment), 90m
        g.add_segment(seg(3, 0, 2, 20.0 / 111_000.0));   // node0 <-> node2 (shortcut), 20m
        g.add_segment(seg(4, 2, 3, 0.001));               // spur: makes node 2 a valid junction
        g.add_segment(seg(5, 2, 4, 0.001));               // spur: makes node 2 a valid junction
        g.add_segment(seg(6, 0, 5, 0.001));               // spur: makes node 0 a valid junction

        let input = LineLocationInput {
            path: vec![SegmentId(1)],
            start_node: NodeId(1),
            start_offset_m: 0.0,
            end_offset_m: 0.0,
            via_split_points: vec![],
        };

        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12)
            .expect("the shortcut leaves from node 0's own LRP, so bearing already handles it");
        let lrps = loc.lrps().unwrap();
        assert_eq!(lrps.len(), 2, "start (at the expanded node 0) + end — no via-point was ever declared");
        assert!((lrps[0].coord.0 - (-0.001)).abs() < 1e-9 && (lrps[0].coord.1 - 0.0).abs() < 1e-9,
            "first LRP should sit at the expanded node 0, not the original invalid node 1");
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

        let err = encode_line(&g, &input, 150.0, 15_000.0, 12)
            .expect_err("a 165° boundary turn must be rejected under the default-style 150° cap");
        assert!(matches!(err, EncodeError::NoRoute), "err={err:?}");

        let loc = encode_line(&g, &input, 180.0, 15_000.0, 12)
            .expect("with the turn-angle gate disabled, the same route should encode successfully");
        // With the gate disabled this graph has exactly one possible path from
        // node 1 to node 3 — no shortcut exists for the via-point to protect
        // against, so the unconstrained sweep reproduces it in a single leg
        // and the declared via-point does not force an extra LRP.
        assert_eq!(loc.lrps().unwrap().len(), 2, "start + end, no via needed");
    }
}
