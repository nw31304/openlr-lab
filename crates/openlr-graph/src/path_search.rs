use std::collections::{BinaryHeap, HashMap};
use std::cmp::Reverse;

use crate::{Graph, NodeId, SegmentId, TileKey};

/// Sentinel `start_seg` for a search with no real incoming edge to bias against
/// (e.g. routing from a bare waypoint that isn't the exit of some prior LRP
/// segment). Never matches a real `SegmentId`, so the "never re-enter the
/// segment we arrived via" rule in `Graph::successors` has no effect at the
/// very first hop.
pub const NO_PRIOR_SEG: SegmentId = SegmentId(u32::MAX);

/// Result of a plain shortest-path search: the ordered segment list from
/// `start_node` to `goal_node`, plus its total length.
#[derive(Debug, Clone)]
pub struct PathResult {
    pub segments: Vec<SegmentId>,
    pub length_m: f64,
}

/// Outcome of `shortest_path`, distinguishing "genuinely no path" from "the
/// search reached a graph boundary whose home tile isn't loaded yet" — the
/// caller must load that tile and retry, exactly like decode-side A*
/// (`astar.rs`) already does via `RoutingFailure::NeedsTile`.
#[derive(Debug, Clone)]
pub enum PathOutcome {
    Found(PathResult),
    NoPath,
    NeedsTile(TileKey),
}

#[derive(Clone, PartialEq)]
struct F64Key(f64);
impl Eq for F64Key {}
impl PartialOrd for F64Key {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
}
impl Ord for F64Key {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.partial_cmp(&o.0).unwrap_or(std::cmp::Ordering::Equal)
    }
}

struct ClosedEntry {
    via_seg: SegmentId,
    g: f64,
    parent: Option<usize>,
}

type OpenElem = (Reverse<F64Key>, F64Key, NodeId, SegmentId, Option<usize>);

/// Plain distance-cost A* over `Graph::successors` — no DNP window, no candidate
/// scoring, no multi-tile boundary handling. This is the shared shortest-path
/// primitive used both by the encoder's coverage sweep (verifying that a
/// shortest path reproduces a given path) and by any caller that just needs
/// "the shortest path between two points on this graph" (e.g. routing a drawn
/// waypoint sequence before it's handed to the encoder).
///
/// Decoding's `astar.rs::find_route` stays a separate, decode-specific
/// implementation (it's shaped around `ScoredCandidate`/`DecodeTrace` and a
/// DNP-derived max-distance cap that only make sense when *verifying* an
/// already-encoded reference) — but it could later be refactored to call this
/// as its inner loop.
///
/// `start_seg` seeds the search as if arrival was via that segment, so the
/// first turn-restriction/turn-angle check at `start_node` is respected exactly
/// as it would be mid-route. Pass `lfrcnp = 7` for an unrestricted search and
/// `max_turn_deviation_deg = 180.0` to disable the turn-angle gate.
///
/// `zoom` is used only for the tile-boundary check (see `PathOutcome`) — it
/// must match the zoom level `graph`'s tiles were loaded at.
pub fn shortest_path(
    graph: &Graph,
    start_node: NodeId,
    start_seg: SegmentId,
    goal_node: NodeId,
    lfrcnp: u8,
    max_turn_deviation_deg: f64,
    max_expansions: usize,
    zoom: u8,
) -> PathOutcome {
    if start_node == goal_node {
        return PathOutcome::Found(PathResult { segments: vec![], length_m: 0.0 });
    }

    let Some((goal_lon, goal_lat)) = graph.nodes.get(&goal_node).map(|n| (n.lon, n.lat)) else {
        return PathOutcome::NoPath;
    };

    let mut closed: HashMap<(NodeId, SegmentId), usize> = HashMap::new();
    let mut closed_list: Vec<ClosedEntry> = Vec::new();
    let mut open: BinaryHeap<OpenElem> = BinaryHeap::new();

    let h0 = graph.node_dist_m(start_node, goal_lon, goal_lat).unwrap_or(0.0);
    open.push((Reverse(F64Key(h0)), F64Key(0.0), start_node, start_seg, None));

    let mut expansions: usize = 0;

    while let Some((_, g_key, node, via_seg, parent_idx)) = open.pop() {
        let g = g_key.0;
        let state = (node, via_seg);

        expansions += 1;
        if max_expansions > 0 && expansions > max_expansions {
            return PathOutcome::NoPath;
        }

        if let Some(&prev_idx) = closed.get(&state) {
            if closed_list[prev_idx].g <= g {
                continue;
            }
        }

        let entry_idx = closed_list.len();
        closed_list.push(ClosedEntry { via_seg, g, parent: parent_idx });
        closed.insert(state, entry_idx);

        if node == goal_node && via_seg != start_seg {
            return PathOutcome::Found(reconstruct(entry_idx, &closed_list, start_seg));
        }

        // Mirrors astar.rs's boundary check: a segment (B->C) is stored in both
        // tile_of(B) and tile_of(C), so outgoing[B] may look complete even when
        // B's home tile isn't loaded — its true successors could be invisible.
        if let Some(n) = graph.nodes.get(&node) {
            if n.is_boundary {
                let tk = TileKey::from_lonlat(n.lon, n.lat, zoom);
                if !graph.is_tile_loaded(tk) {
                    return PathOutcome::NeedsTile(tk);
                }
            }
        }

        for (next_node, next_seg, seg_len) in graph.successors(node, via_seg, lfrcnp, max_turn_deviation_deg) {
            let new_g = g + seg_len;
            let next_state = (next_node, next_seg);
            if let Some(&prev_idx) = closed.get(&next_state) {
                if closed_list[prev_idx].g <= new_g {
                    continue;
                }
            }
            let h = graph.node_dist_m(next_node, goal_lon, goal_lat).unwrap_or(0.0);
            open.push((Reverse(F64Key(new_g + h)), F64Key(new_g), next_node, next_seg, Some(entry_idx)));
        }
    }

    PathOutcome::NoPath
}

/// Reconstruct the ordered segment list from the closed list. `start_seg` is
/// excluded — it's the caller's own arrival edge, not part of the path found.
fn reconstruct(goal_idx: usize, closed_list: &[ClosedEntry], start_seg: SegmentId) -> PathResult {
    let mut segs: Vec<SegmentId> = Vec::new();
    let mut idx = goal_idx;
    let length_m = closed_list[goal_idx].g;
    loop {
        let entry = &closed_list[idx];
        if entry.via_seg != start_seg {
            segs.push(entry.via_seg);
        }
        match entry.parent {
            Some(p) => idx = p,
            None => break,
        }
    }
    segs.reverse();
    PathResult { segments: segs, length_m }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, NetworkNode, NetworkSegment};

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

    fn unwrap_found(outcome: PathOutcome) -> PathResult {
        match outcome {
            PathOutcome::Found(r) => r,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn finds_direct_path() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));

        let result = unwrap_found(shortest_path(&g, NodeId(0), NO_PRIOR_SEG, NodeId(2), 7, 180.0, 0, 12));
        assert_eq!(result.segments, vec![SegmentId(1), SegmentId(2)]);
        assert!((result.length_m - 200.0).abs() < 1e-6);
    }

    #[test]
    fn no_path_returns_no_path() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.1, 0.1));
        g.add_segment(seg(1, 0, 1, 100.0));
        // node 2 is disconnected
        assert!(matches!(
            shortest_path(&g, NodeId(0), NO_PRIOR_SEG, NodeId(2), 7, 180.0, 0, 12),
            PathOutcome::NoPath
        ));
    }

    #[test]
    fn trivial_same_node() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        let result = unwrap_found(shortest_path(&g, NodeId(0), NO_PRIOR_SEG, NodeId(0), 7, 180.0, 0, 12));
        assert!(result.segments.is_empty());
        assert_eq!(result.length_m, 0.0);
    }

    #[test]
    fn respects_incoming_seg_u_turn_rule() {
        // Arriving at node 1 via seg 1: the search must not re-enter seg 1
        // immediately, but should still find the path onward via seg 2.
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));
        let result = unwrap_found(shortest_path(&g, NodeId(1), SegmentId(1), NodeId(2), 7, 180.0, 0, 12));
        assert_eq!(result.segments, vec![SegmentId(2)]);
    }

    #[test]
    fn stops_at_unloaded_boundary_tile() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        let mut boundary = node(1, 0.001, 0.0);
        boundary.is_boundary = true;
        g.add_node(boundary);
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));
        // Note: the boundary node's home tile is never marked loaded here.
        match shortest_path(&g, NodeId(0), NO_PRIOR_SEG, NodeId(2), 7, 180.0, 0, 12) {
            PathOutcome::NeedsTile(_) => {}
            other => panic!("expected NeedsTile, got {other:?}"),
        }
    }
}
