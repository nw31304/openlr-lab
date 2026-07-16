//! Table 54 Steps 3-6: the forward coverage sweep. Not a bisection — each
//! round searches shortest-path from the (advancing) current start all the
//! way to the location's *fixed* end; on divergence, an intermediate LRP is
//! dropped at the last point of agreement (preferring a valid node there) and
//! the search restarts from there to the same end.

use openlr_graph::{shortest_path, Graph, NodeId, PathOutcome, SegmentId};

use crate::EncodeError;

/// One leg of the eventual location reference.
#[derive(Debug)]
pub struct Leg {
    pub start_node: NodeId,
    pub segments: Vec<SegmentId>,
}

/// Sweep `path` (the full desired route, already expanded to valid start/end
/// nodes) into legs, inserting intermediate LRPs wherever necessary so that
/// each leg is independently reproducible by a shortest-path search. Every
/// round searches from the (advancing) current position all the way to the
/// *fixed* `end_node`, regardless of how many waypoints lie ahead — an LRP
/// only ever gets inserted where the search actually diverges from `path`,
/// never merely because a waypoint happens to sit there (waypoints pin down
/// which segments the location covers, not where LRPs are placed).
///
/// `start_seg` biases the very first search the same way A* seeds an
/// incoming segment for turn-angle purposes: pass `NO_PRIOR_SEG` when there's
/// no "before" to compare against (the location's own overall start).
///
/// `waypoint_boundaries` is a recovery mechanism, not a splitting rule: each
/// entry is a segment-count offset into `path` where a user-drawn waypoint
/// falls (strictly increasing, each in `1..path.len()`; pass `&[]` for a
/// plain two-endpoint route with no interior waypoints). It is consulted
/// *only* when the search toward `end_node` diverges from `path` with zero
/// prefix agreement — i.e. the very next segment isn't part of any route to
/// the fixed end at all, so there is no valid divergence point to split at
/// (see `best_intermediate_position`). In that specific case, retargeting
/// the search at the next waypoint instead is guaranteed to succeed (Layer 1
/// already found a route there via genuine shortest-path search, and by
/// Dijkstra's optimal-substructure property re-deriving it from the same
/// state can't disagree) — this forces just enough progress to get unstuck,
/// after which the sweep immediately resumes targeting the fixed `end_node`
/// again, so any waypoints beyond the recovery point still only get an LRP
/// if they too turn out to be necessary.
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
    waypoint_boundaries: &[usize],
) -> Result<Vec<Leg>, EncodeError> {
    let mut legs = Vec::new();
    let mut remaining = path;
    let mut current_start = start_node;
    let mut current_start_seg = start_seg;
    let mut consumed = 0usize;

    loop {
        let result = match shortest_path(graph, current_start, current_start_seg, end_node, 7, max_turn_deviation_deg, 0, zoom) {
            PathOutcome::Found(r) => r,
            PathOutcome::NoPath => return Err(EncodeError::NoRoute),
            PathOutcome::NeedsTile(tk) => return Err(EncodeError::NeedsTile(tk)),
        };

        let agreement = common_prefix_len(remaining, &result.segments);

        if agreement == remaining.len() {
            // Rule-1 applies here too, not just on the split-required branch
            // below — a perfectly-reproducible leg can still be longer than
            // `max_leg_m` allows (this was never reachable when the cap was
            // always the architecture's 15km ceiling, since a drawn route
            // rarely goes that far without a natural via-point, but a
            // caller-supplied smaller cap hits it routinely).
            let leg_len_m: f64 = remaining.iter()
                .filter_map(|id| graph.segments.get(id))
                .map(|s| s.length_m)
                .sum();
            if leg_len_m > max_leg_m {
                return Err(EncodeError::LegTooLong { length_m: leg_len_m, max_leg_m });
            }
            legs.push(Leg { start_node: current_start, segments: remaining.to_vec() });
            return Ok(legs);
        }

        let raw_split = best_intermediate_position(graph, current_start, remaining, agreement);
        let split_at = match raw_split {
            Some(k) if k > 0 => k,
            // Zero prefix agreement — the search toward `end_node` doesn't
            // even take `remaining`'s first segment. No divergence point
            // exists to split at; fall back to the next waypoint ahead, if
            // any, as a one-off recovery target.
            _ => match waypoint_boundaries.iter().find(|&&b| b > consumed) {
                Some(&boundary) => boundary - consumed,
                None => return Err(EncodeError::NoRoute),
            },
        };

        let leg_segments = remaining[..split_at].to_vec();
        let intermediate_node = trace_end_node(graph, current_start, &leg_segments)
            .ok_or(EncodeError::NoRoute)?;
        let last_seg_of_leg = leg_segments[leg_segments.len() - 1];

        // Rule-1: split further if this leg alone exceeds the 15km cap. (v1
        // scope: report an error rather than the spec's full virtual-point
        // splitting — real inputs built from a routed path rarely produce a
        // single leg this long before an intermediate is otherwise needed.)
        let leg_len_m: f64 = leg_segments.iter()
            .filter_map(|id| graph.segments.get(id))
            .map(|s| s.length_m)
            .sum();
        if leg_len_m > max_leg_m {
            return Err(EncodeError::LegTooLong { length_m: leg_len_m, max_leg_m });
        }

        legs.push(Leg { start_node: current_start, segments: leg_segments });
        remaining = &remaining[split_at..];
        consumed += split_at;
        current_start = intermediate_node;
        current_start_seg = last_seg_of_leg;
    }
}

/// Length of the shared prefix of `a` and `b`.
fn common_prefix_len(a: &[SegmentId], b: &[SegmentId]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// The node reached by walking `segments` in order, starting from `node`.
pub(crate) fn trace_end_node(graph: &Graph, mut node: NodeId, segments: &[SegmentId]) -> Option<NodeId> {
    for seg_id in segments {
        node = other_end(graph, *seg_id, node)?;
    }
    Some(node)
}

/// The node at the far end of `seg_id` from `entered_from`.
fn other_end(graph: &Graph, seg_id: SegmentId, entered_from: NodeId) -> Option<NodeId> {
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
        let legs = sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(2), 15_000.0, 180.0, 12, &[]).unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].segments, path);
        assert_eq!(legs[0].start_node, NodeId(0));
    }

    #[test]
    fn straight_path_over_max_leg_cap_is_rejected() {
        // Same shape as straight_path_needs_no_intermediates (a perfectly
        // reproducible, undivergent path) — but with a `max_leg_m` well below
        // its 200m total. The "no divergence" success branch must still
        // enforce Rule-1, not just the "had to split" branch.
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));

        let path = vec![SegmentId(1), SegmentId(2)];
        match sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(2), 50.0, 180.0, 12, &[]) {
            Err(EncodeError::LegTooLong { length_m, max_leg_m }) => {
                assert!((length_m - 200.0).abs() < 1e-6);
                assert_eq!(max_leg_m, 50.0);
            }
            other => panic!("expected LegTooLong (200m path over a 50m cap with no divergence), got {other:?}"),
        }
    }

    #[test]
    fn only_the_necessary_waypoint_boundary_is_used_as_a_recovery_target() {
        // 0 --seg1--> 1 --seg2--> 2 --seg3--> 3, plus a direct shortcut
        // seg4: 0 -> 2 (10m) that bypasses node 1 entirely. Two waypoints
        // are declared — at node 1 (boundary 1) and at node 2 (boundary
        // 2) — but only node 1 is actually needed to keep the sweep off the
        // shortcut; once resumed from there, the rest of the route
        // (1->2->3) has no further shortcut, so node 2's declared boundary
        // should never be consulted. Waypoints only select which segments
        // the location covers — an "overly specified" extra one shouldn't
        // cost an extra LRP.
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
        let legs = sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(3), 15_000.0, 180.0, 12, &[1, 2]).unwrap();

        assert_eq!(legs.len(), 2, "only the node-1 recovery should have been needed, not both declared waypoints");
        assert_eq!(legs[0].segments, vec![SegmentId(1)]);
        assert_eq!(legs[0].start_node, NodeId(0));
        assert_eq!(legs[1].segments, vec![SegmentId(2), SegmentId(3)]);
        assert_eq!(legs[1].start_node, NodeId(1));
    }

    // NOTE on why there's no synthetic "a shortcut forces an intermediate"
    // test here: by Dijkstra/A*'s optimal-substructure property, if segment Y
    // genuinely beats segment X for reaching the *same fixed far end* from a
    // given (node, incoming_seg) state, restarting the search from that exact
    // state (as an intermediate LRP) makes the identical choice again — a true
    // shortcut can never be "fixed" by splitting, no matter where you split.
    // The whitepaper's own Step 7 note confirms intermediate insertion is
    // really for *heuristic-search quirks and ties* ("if the encoder uses a
    // heuristic function... this heuristic leads the search to the end of the
    // location but not along the location itself"), not for overriding a
    // genuinely shorter competing route. That's inherently sensitive to
    // implementation-level tie-breaking, so it's exercised by the real-map
    // round-trip test rather than a contrived synthetic graph here.

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
