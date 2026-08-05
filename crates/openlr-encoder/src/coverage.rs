//! Table 54 Steps 3-6: the forward coverage sweep. Not a bisection — each leg
//! starts at an LRP whose bearing/FRC/FOW already commit the decoder to a
//! specific first hop, so no competing route can ever "win" that hop away.
//! The sweep walks forward past that hop, and past any further nodes with no
//! real alternative of their own (Rule-4's "invalid" — a pure pass-through),
//! before ever asking whether a competing shortest path exists to disagree
//! with the rest. Only once a genuine decision point (a valid node with real
//! alternatives) is reached does an actual search run, and only a divergence
//! found *there* ever drops an intermediate LRP.

use openlr_graph::{shortest_path, Graph, NodeId, PathOutcome, SegmentId, NO_PRIOR_SEG};

use crate::virtual_split::{self, LrpAnchor};
use crate::EncodeError;

/// One leg of the eventual location reference.
#[derive(Debug)]
pub struct Leg {
    pub start: LrpAnchor,
    /// Full segment IDs spanned by this leg, in travel order — the first
    /// and/or last entry may be only partially covered when `start` (or the
    /// next leg's own `start`) is `LrpAnchor::Virtual`; `length_m` is the
    /// authoritative distance, not a re-sum of these segments' full lengths.
    pub segments: Vec<SegmentId>,
    pub length_m: f64,
}

/// Sweep `path` (the full desired route, already expanded to valid start/end
/// nodes) into legs, inserting intermediate LRPs wherever necessary so that
/// each leg is independently reproducible by a shortest-path search.
///
/// Every leg's start is an LRP, and an LRP's bearing/FRC/FOW are drawn from
/// the segment it should point the decoder along next — so a leg's own
/// first hop, and any further hop through a node with no real alternative,
/// is never in competition with some other route the decoder might prefer;
/// it isn't a candidate the decoder even considers. `sweep_coverage` never
/// runs a comparison search over that forced ground (see
/// `forced_prefix_len`) — only once it reaches the first node offering a
/// genuine choice does it search, targeting the *fixed* `end_node` from
/// there. An LRP is inserted only where that search actually diverges from
/// `path` — never merely because a waypoint happens to sit somewhere
/// (waypoints pin down which segments the location covers, not where LRPs
/// are placed).
///
/// `start_seg` is the incoming segment at `start_node`, for turn-angle
/// purposes — pass `NO_PRIOR_SEG` when there's no "before" to compare
/// against (the location's own overall start).
///
/// `max_turn_deviation_deg` is the same turn-angle cap decode-side A* uses
/// (`DecodeParams::max_interior_turn_deviation_deg`) — despite the decode-only
/// name, it governs both directions: an encoder that would happily route
/// across a physically-impossible U-turn (e.g. a dead end forcing a "walk
/// back across the segment just arrived on") would produce a reference no
/// real navigation system could sensibly reproduce. Pass `180.0` to disable.
pub fn sweep_coverage(
    graph: &Graph,
    path: &[SegmentId],
    start_node: NodeId,
    start_seg: SegmentId,
    end_node: NodeId,
    max_leg_m: f64,
    max_turn_deviation_deg: f64,
    zoom: u8,
) -> Result<Vec<Leg>, EncodeError> {
    let mut legs = Vec::new();
    let mut remaining = path;
    let mut current_start = start_node;
    let mut current_start_seg = start_seg;

    loop {
        let forced = forced_prefix_len(graph, current_start, current_start_seg, remaining, max_turn_deviation_deg)
            .ok_or(EncodeError::NoRoute)?;

        if forced == remaining.len() {
            // Rule-1 applies here too, not just on the split-required branch
            // below — a perfectly-reproducible leg can still be longer than
            // `max_leg_m` allows (this was never reachable when the cap was
            // always the architecture's 15km ceiling, since a drawn route
            // rarely goes that far without a natural via-point, but a
            // caller-supplied smaller cap hits it routinely). `split_for_rule1`
            // subdivides it (inserting virtual-point LRPs if needed) rather
            // than erroring — see its own doc comment for why no additional
            // A*/divergence check is needed for those new split points.
            legs.extend(virtual_split::split_for_rule1(graph, current_start, remaining, max_leg_m)?);
            return Ok(legs);
        }

        // Beyond the forced ground, a real decision point has been reached —
        // now it's meaningful to ask what a shortest-path search targeting
        // the fixed end would actually do from here.
        let anchor = trace_end_node(graph, current_start, &remaining[..forced])
            .ok_or(EncodeError::NoRoute)?;
        let anchor_incoming_seg = remaining[forced - 1];
        let tail = &remaining[forced..];

        let result = match shortest_path(graph, anchor, anchor_incoming_seg, end_node, 7, max_turn_deviation_deg, 0, zoom) {
            PathOutcome::Found(r) => r,
            PathOutcome::NoPath => return Err(EncodeError::NoRoute),
            PathOutcome::NeedsTile(tk) => return Err(EncodeError::NeedsTile(tk)),
        };

        let agreement = common_prefix_len(tail, &result.segments);

        if agreement == tail.len() {
            legs.extend(virtual_split::split_for_rule1(graph, current_start, remaining, max_leg_m)?);
            return Ok(legs);
        }

        // `anchor` is already a confirmed-valid node — that's why the forced
        // walk stopped there — so unlike a raw divergence measured from
        // `current_start` itself (whose own first hop is masked by that
        // LRP's own bearing), zero agreement here is a real, legitimate
        // split point, not a degenerate one requiring a separate recovery
        // mechanism.
        let split_within_tail = best_intermediate_position(graph, anchor, tail, agreement)
            .expect("best_intermediate_position always returns Some");
        let split_at = forced + split_within_tail;

        let leg_segments = remaining[..split_at].to_vec();
        let intermediate_node = trace_end_node(graph, current_start, &leg_segments)
            .ok_or(EncodeError::NoRoute)?;
        let last_seg_of_leg = leg_segments[leg_segments.len() - 1];

        // Rule-1: split further if this leg alone exceeds the 15km cap
        // (inserting virtual-point LRPs if needed). This operates on the raw,
        // unsplit `leg_segments` -- `intermediate_node`/`last_seg_of_leg`
        // above (which drive the *outer* divergence-protection loop's next
        // iteration) are computed from that same raw list regardless of how
        // many pieces `split_for_rule1` produces internally.
        legs.extend(virtual_split::split_for_rule1(graph, current_start, &leg_segments, max_leg_m)?);

        remaining = &remaining[split_at..];
        current_start = intermediate_node;
        current_start_seg = last_seg_of_leg;
    }
}

/// Walks forward from `start_node` along `remaining`, always taking at least
/// one hop — the leg's own bearing-selected first segment is never in
/// competition, see the module doc — then continuing through any further
/// nodes with no real alternative of their own (Rule-4's "invalid": a pure
/// pass-through), stopping at the first node that either offers a genuine
/// choice or is the end of `remaining` (which is always `end_node`, itself
/// always valid). Validates turn-angle/U-turn legality of each hop along the
/// way exactly as `Graph::successors` would — this ground is never covered
/// by an actual search, so nothing else checks it. Returns `None` if the
/// walk hits a disconnected segment or an illegal turn.
fn forced_prefix_len(
    graph: &Graph,
    start_node: NodeId,
    start_seg: SegmentId,
    remaining: &[SegmentId],
    max_turn_deviation_deg: f64,
) -> Option<usize> {
    let mut node = start_node;
    let mut incoming = start_seg;
    let mut pos = 0;
    while pos < remaining.len() {
        let next_seg = remaining[pos];
        if incoming != NO_PRIOR_SEG {
            if next_seg == incoming {
                return None; // exact U-turn -- never legal, regardless of angle
            }
            if max_turn_deviation_deg < 180.0 {
                if let Some(dev) = graph.turn_deviation_deg(incoming, node, next_seg) {
                    if dev > max_turn_deviation_deg {
                        return None;
                    }
                }
            }
        }
        node = other_end(graph, next_seg, node)?;
        incoming = next_seg;
        pos += 1;
        if pos == remaining.len() || graph.is_valid_node(node) {
            return Some(pos);
        }
    }
    Some(pos)
}

/// Length of the shared prefix of `a` and `b`.
fn common_prefix_len(a: &[SegmentId], b: &[SegmentId]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// The node reached by walking `segments` in order, starting from `node`.
///
/// Topology-only (ignores `Direction`) — safe here because every current
/// caller already only ever passes a path whose direction legality is
/// guaranteed by construction: A*-routed (`shortest_path` respects
/// `Direction`) or backward search over a path already validated elsewhere
/// (`best_intermediate_position`). For a path that arrives from *outside*
/// this crate with no such guarantee (e.g. a caller-specified segment
/// list), use `trace_end_node_validated` instead — see CLAUDE.md
/// Invariant 10 for why this distinction matters.
pub(crate) fn trace_end_node(graph: &Graph, mut node: NodeId, segments: &[SegmentId]) -> Option<NodeId> {
    for seg_id in segments {
        node = other_end(graph, *seg_id, node)?;
    }
    Some(node)
}

/// Like `trace_end_node`, but for untrusted input: also checks that each
/// segment is actually departable from the node the walk arrived at
/// (`Graph::outgoing_segments`), not just that it touches it. Distinguishes
/// "doesn't connect at all" (`EncodeError::Disconnected`) from "connects,
/// but only in the direction this path can't use"
/// (`EncodeError::IllegalDirection`) so a caller gets an actionable error
/// rather than a silently mis-encoded reference.
pub(crate) fn trace_end_node_validated(
    graph: &Graph,
    mut node: NodeId,
    segments: &[SegmentId],
) -> Result<NodeId, crate::EncodeError> {
    for (index, &seg_id) in segments.iter().enumerate() {
        if graph.outgoing_segments(node).contains(&seg_id) {
            // `outgoing_segments` already implies `other_end` will succeed.
            node = other_end(graph, seg_id, node).expect("outgoing_segments implies connectivity");
            continue;
        }
        let touches_node = graph.segments.get(&seg_id)
            .is_some_and(|seg| seg.start_node == node || seg.end_node == node);
        return Err(if touches_node {
            crate::EncodeError::IllegalDirection { index, segment: seg_id }
        } else {
            crate::EncodeError::Disconnected { index }
        });
    }
    Ok(node)
}

/// The node at the far end of `seg_id` from `entered_from`.
pub(crate) fn other_end(graph: &Graph, seg_id: SegmentId, entered_from: NodeId) -> Option<NodeId> {
    let seg = graph.segments.get(&seg_id)?;
    if seg.start_node == entered_from {
        Some(seg.end_node)
    } else if seg.end_node == entered_from {
        Some(seg.start_node)
    } else {
        None
    }
}

/// Table 54 Step 5: scan backward from the raw divergence point (`agreement`)
/// for the nearest position whose node is valid, never further back than the
/// leg's own start (position 1). Falls back to the raw divergence point itself
/// (possibly an invalid node) if nothing valid is found, per the spec's
/// explicit escape hatch.
fn best_intermediate_position(
    graph: &Graph,
    start_node: NodeId,
    remaining: &[SegmentId],
    agreement: usize,
) -> Option<usize> {
    for k in (1..=agreement).rev() {
        if let Some(node) = trace_end_node(graph, start_node, &remaining[..k]) {
            if graph.is_valid_node(node) {
                return Some(k);
            }
        }
    }
    Some(agreement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, NetworkNode, NetworkSegment, NO_PRIOR_SEG};

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: String::new(), is_boundary: false }
    }
    fn seg(id: u32, s: u32, e: u32, len: f64) -> NetworkSegment {
        NetworkSegment {
            id: SegmentId(id),
            start_node: NodeId(s),
            end_node: NodeId(e),
            geometry: vec![(0.0, 0.0), (0.001, 0.0)],
            length_m: len,
            frc: 3, fow: 3,
            direction: Direction::Both,
            stable_id: String::new(),
        }
    }

    #[test]
    fn straight_path_needs_no_intermediates() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));

        let path = vec![SegmentId(1), SegmentId(2)];
        let legs = sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(2), 15_000.0, 180.0, 12).unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].segments, path);
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
    }

    #[test]
    fn straight_path_over_max_leg_cap_is_split_via_virtual_points() {
        // Same shape as straight_path_needs_no_intermediates (a perfectly
        // reproducible, undivergent path) — but with a `max_leg_m` well below
        // its 200m total. The "no divergence" success branch must still
        // enforce Rule-1, not just the "had to split" branch -- but instead
        // of erroring, it now subdivides via `virtual_split::split_for_rule1`
        // (no real node exists within a 50m budget on either 100m segment,
        // so every split here is a virtual, mid-segment point).
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));

        let path = vec![SegmentId(1), SegmentId(2)];
        let legs = sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(2), 50.0, 180.0, 12).unwrap();

        assert_eq!(legs.len(), 4, "200m over a 50m cap needs 3 splits");
        for leg in &legs {
            assert!((leg.length_m - 50.0).abs() < 1e-6, "length_m={}", leg.length_m);
        }
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
        assert!(matches!(legs[1].start, LrpAnchor::Virtual { .. }), "50m into the 100m seg1 -- no node there");
        assert!(matches!(legs[2].start, LrpAnchor::Node(NodeId(1))), "the next 50m finishes seg1, landing exactly on node 1");
        assert!(matches!(legs[3].start, LrpAnchor::Virtual { .. }), "50m into the 100m seg2 -- no node there");
    }

    #[test]
    fn shortcut_off_the_first_lrp_needs_no_intermediate() {
        // 0 --seg1--> 1 --seg2--> 2 --seg3--> 3, plus a direct shortcut
        // seg4: 0 -> 2 (10m) that bypasses node 1 entirely. The shortcut
        // leaves from node 0 itself — the very node the leg's own LRP sits
        // on — so its bearing already commits the decoder to seg1, not
        // seg4; the shortcut is never a candidate the decoder considers, and
        // no intermediate LRP is needed to protect against it. The forced
        // walk covers seg1 (bearing-selected) and seg2 (node 1 is a plain
        // pass-through, no real alternative) before ever reaching a real
        // decision point (node 2, valid via seg3/seg4), at which point the
        // rest of the route (seg3 to the fixed end) has no further shortcut
        // to disagree with either.
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_node(node(3, 0.003, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));
        g.add_segment(seg(3, 2, 3, 100.0));
        g.add_segment(seg(4, 0, 2, 10.0)); // shortcut bypassing node 1

        let path = vec![SegmentId(1), SegmentId(2), SegmentId(3)];
        let legs = sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(3), 15_000.0, 180.0, 12).unwrap();

        assert_eq!(legs.len(), 1, "the shortcut leaves from the LRP's own node, so bearing already handles it");
        assert_eq!(legs[0].segments, path);
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
    }

    #[test]
    fn shortcut_past_a_real_junction_does_need_an_intermediate() {
        // 0 --seg1--> 1 --seg2--> 2 --seg3--> 3, with node 1 a *real* 3-way
        // junction (extra spur seg9) and a direct shortcut seg4: 1 -> 3 (10m)
        // that bypasses node 2. Unlike the shortcut-off-the-first-LRP case,
        // this shortcut leaves from node 1 — a node reached only *after* the
        // forced walk (seg1 is bearing-selected departing node 0, but node 1
        // itself offers a genuine alternative, so it's a real decision
        // point, not forced ground). A real intermediate LRP is needed there
        // to steer the decoder onto seg2 instead of the seg4 shortcut.
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_node(node(3, 0.003, 0.0));
        g.add_node(node(4, 0.001, 0.001)); // spur off node 1
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));
        g.add_segment(seg(3, 2, 3, 100.0));
        g.add_segment(seg(4, 1, 3, 10.0)); // shortcut bypassing node 2
        g.add_segment(seg(9, 1, 4, 100.0)); // spur: makes node 1 a valid 3-way junction

        let path = vec![SegmentId(1), SegmentId(2), SegmentId(3)];
        let legs = sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(3), 15_000.0, 180.0, 12).unwrap();

        assert_eq!(legs.len(), 2, "node 1 is a real decision point past the forced ground, so its shortcut needs protecting");
        assert_eq!(legs[0].segments, vec![SegmentId(1)]);
        assert!(matches!(legs[0].start, LrpAnchor::Node(NodeId(0))));
        assert_eq!(legs[1].segments, vec![SegmentId(2), SegmentId(3)]);
        assert!(matches!(legs[1].start, LrpAnchor::Node(NodeId(1))));
    }

    #[test]
    fn common_prefix_len_stops_at_first_mismatch() {
        let a = [SegmentId(1), SegmentId(2), SegmentId(3)];
        let b = [SegmentId(1), SegmentId(2), SegmentId(9)];
        assert_eq!(common_prefix_len(&a, &b), 2);
    }

    #[test]
    fn trace_end_node_walks_segments_in_order() {
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));
        let end = trace_end_node(&g, NodeId(0), &[SegmentId(1), SegmentId(2)]);
        assert_eq!(end, Some(NodeId(2)));
    }

    #[test]
    fn other_end_handles_either_direction() {
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 100.0));
        assert_eq!(other_end(&g, SegmentId(1), NodeId(0)), Some(NodeId(1)));
        assert_eq!(other_end(&g, SegmentId(1), NodeId(1)), Some(NodeId(0)));
        assert_eq!(other_end(&g, SegmentId(1), NodeId(9)), None);
    }

    #[test]
    fn trace_end_node_validated_accepts_a_legally_directed_path() {
        let mut g = Graph::new();
        g.add_segment(NetworkSegment {
            id: SegmentId(1), start_node: NodeId(0), end_node: NodeId(1),
            geometry: vec![(0.0, 0.0), (0.001, 0.0)], length_m: 100.0,
            frc: 3, fow: 3, direction: Direction::Forward, stable_id: String::new(),
        });
        g.add_segment(seg(2, 1, 2, 100.0)); // Both -- departable either way
        let end = trace_end_node_validated(&g, NodeId(0), &[SegmentId(1), SegmentId(2)]).unwrap();
        assert_eq!(end, NodeId(2));
    }

    #[test]
    fn trace_end_node_validated_rejects_a_wrong_direction_one_way() {
        // Seg 1 is one-way Forward (0->1 only) -- walking it starting from
        // node 1 requires illegal travel, even though it topologically
        // touches node 1 (trace_end_node/other_end would happily accept it).
        let mut g = Graph::new();
        g.add_segment(NetworkSegment {
            id: SegmentId(1), start_node: NodeId(0), end_node: NodeId(1),
            geometry: vec![(0.0, 0.0), (0.001, 0.0)], length_m: 100.0,
            frc: 3, fow: 3, direction: Direction::Forward, stable_id: String::new(),
        });
        let err = trace_end_node_validated(&g, NodeId(1), &[SegmentId(1)]).unwrap_err();
        assert!(matches!(err, crate::EncodeError::IllegalDirection { index: 0, segment: SegmentId(1) }));
    }

    #[test]
    fn trace_end_node_validated_reports_genuine_disconnection_separately() {
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 100.0));
        // Segment 2 doesn't touch node 1 at all (it's over on the 5-6 pair).
        g.add_segment(seg(2, 5, 6, 100.0));
        let err = trace_end_node_validated(&g, NodeId(0), &[SegmentId(1), SegmentId(2)]).unwrap_err();
        assert!(matches!(err, crate::EncodeError::Disconnected { index: 1 }));
    }

    #[test]
    fn best_intermediate_position_prefers_a_valid_node() {
        // 0 -1-> 1 -2-> 2 -3-> 3, with node 2 invalid (pure pass-through) and
        // node 1 a real branch (valid). If agreement only reaches node 2 (a
        // pass-through), the search should back up to node 1 instead.
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));
        g.add_segment(seg(3, 2, 3, 100.0));
        g.add_segment(seg(9, 1, 4, 100.0)); // extra branch makes node 1 valid
        let remaining = [SegmentId(1), SegmentId(2), SegmentId(3)];
        let pos = best_intermediate_position(&g, NodeId(0), &remaining, 2).unwrap();
        assert_eq!(pos, 1, "should back up to node 1 (valid) rather than stop at node 2 (pass-through)");
    }
}
