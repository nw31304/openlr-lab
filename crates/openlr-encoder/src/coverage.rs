//! Table 54 Steps 3-6: the forward coverage sweep. Not a bisection — each
//! round searches shortest-path from the (advancing) current start all the
//! way to the location's *fixed* end; on divergence, an intermediate LRP is
//! dropped at the last point of agreement (preferring a valid node there) and
//! the search restarts from there to the same end.

use openlr_graph::{shortest_path, Graph, NodeId, PathOutcome, SegmentId};

use crate::EncodeError;

/// One leg of the eventual location reference.
pub struct Leg {
    pub start_node: NodeId,
    pub segments: Vec<SegmentId>,
}

/// Sweep `path` (the full desired route, already expanded to valid start/end
/// nodes) into legs, inserting intermediate LRPs wherever necessary so that
/// each leg is independently reproducible by a shortest-path search.
///
/// `start_seg` biases the very first search the same way A* seeds an
/// incoming segment for turn-angle purposes: pass `NO_PRIOR_SEG` when `path`
/// is the whole location (no "before" to compare against), or the last
/// segment of whatever led into `start_node` when sweeping a single
/// waypoint-leg chunk of a larger via-pointed route (`line::encode_line`) —
/// otherwise the turn-angle check below has nothing to compare the chunk's
/// first hop against, silently admitting a sharp turn at the via-point
/// boundary that a single, unchunked sweep would have caught.
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
        let result = match shortest_path(graph, current_start, current_start_seg, end_node, 7, max_turn_deviation_deg, 0, zoom) {
            PathOutcome::Found(r) => r,
            PathOutcome::NoPath => return Err(EncodeError::NoRoute),
            PathOutcome::NeedsTile(tk) => return Err(EncodeError::NeedsTile(tk)),
        };

        let agreement = common_prefix_len(remaining, &result.segments);

        if agreement == remaining.len() {
            legs.push(Leg { start_node: current_start, segments: remaining.to_vec() });
            return Ok(legs);
        }

        let split_at = best_intermediate_position(graph, current_start, remaining, agreement)
            .ok_or(EncodeError::NoRoute)?;
        if split_at == 0 {
            return Err(EncodeError::NoRoute);
        }

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
            return Err(EncodeError::NoRoute);
        }

        legs.push(Leg { start_node: current_start, segments: leg_segments });
        remaining = &remaining[split_at..];
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
        let legs = sweep_coverage(&g, &path, NodeId(0), NO_PRIOR_SEG, NodeId(2), 15_000.0, 180.0, 12).unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].segments, path);
        assert_eq!(legs[0].start_node, NodeId(0));
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
