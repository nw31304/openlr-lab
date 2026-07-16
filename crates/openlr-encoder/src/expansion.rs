//! Rule-4 expansion: walk outward from a location's start/end node to the
//! nearest valid node, per the whitepaper's Figure 27 composition
//! (`final_offset = original_within-leg_offset + expansion_distance`).
//! Expansion never shrinks the location — only ever walks away from it.

use openlr_graph::{Graph, NodeId, SegmentId};

/// Why `expand_to_valid_node` stopped where it did — the LLM-diagnostic
/// counterpart to the plain `Expansion` fields, naming which of the walk's
/// four possible stopping conditions actually fired.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExpansionStopReason {
    /// `start` was already a valid node — no walk needed.
    AlreadyValid,
    /// Walked to a node with more than one continuation (a real junction)
    /// or a dead end recognized by `Graph::is_valid_node` itself.
    ReachedValidNode,
    /// The walk ran out of topology (no segment other than the one just
    /// arrived on touches the current node) while that node was still
    /// invalid — a defensive case; in practice `Graph::is_valid_node`
    /// already treats a true topological dead end as valid, so this
    /// shouldn't fire on well-formed graphs.
    DeadEnd,
    /// Hit `max_leg_m` before reaching a valid node.
    BudgetExhausted,
    /// The next hop's turn angle exceeded `max_turn_deviation_deg` — see the
    /// function doc comment for why this stops the walk rather than being a
    /// gate with an alternative to fall back on.
    SharpTurn { deviation_deg: f64 },
}

/// Result of walking outward from one end of a location to a valid node.
pub struct Expansion {
    pub node: NodeId,
    /// Distance walked past the original boundary node. Zero if it was
    /// already valid.
    pub distance_m: f64,
    /// Segments walked, in the order traversed (from the original boundary
    /// node outward) — empty if no expansion was needed. The caller splices
    /// these into the full path: reversed and prepended for a start-side
    /// expansion, appended as-is for an end-side expansion.
    pub segments: Vec<SegmentId>,
    /// Why the walk stopped — `node` is still invalid unless this is
    /// `AlreadyValid` or `ReachedValidNode`.
    pub stopped: ExpansionStopReason,
}

/// Walk outward from `start` until reaching a valid node, the `max_leg_m` cap
/// (Rule-1), a dead end (no further neighbor), or a turn sharper than
/// `max_turn_deviation_deg` — each accepted as the spec's explicit escape
/// hatch for "no valid node reachable" (see below for why the turn-angle
/// case belongs in that same bucket).
///
/// `skip_seg` is the segment the location continues into from `start` — i.e.
/// the direction *not* to walk (that's the location's own interior). Each
/// subsequent hop then skips whichever segment was just traversed.
///
/// A pass-through node has, by construction, exactly one continuation — no
/// alternative to choose between, so there's nothing for a turn-angle *gate*
/// to protect against here (unlike A*/`sweep_coverage`, where the same check
/// rejects a sharp turn in favor of a better-angled alternative route). But
/// walking through one anyway when its continuation is a genuinely sharp
/// real-world kink is worse than pointless: `sweep_coverage` re-verifies this
/// exact stretch afterward with a real turn-angle search, and *would* reject
/// it there — with no alternative to fall back on, since it's the only
/// physical continuation — surfacing as a confusing generic `NoRoute` with no
/// indication the boundary expansion was the actual cause. Stopping here
/// instead, and accepting the current (possibly still-invalid) node, keeps
/// the failure mode honest: "no valid node reachable without an unnavigable
/// turn" rather than an opaque downstream routing failure.
pub fn expand_to_valid_node(
    graph: &Graph,
    start: NodeId,
    skip_seg: SegmentId,
    max_leg_m: f64,
    max_turn_deviation_deg: f64,
) -> Expansion {
    if graph.is_valid_node(start) {
        return Expansion {
            node: start, distance_m: 0.0, segments: Vec::new(),
            stopped: ExpansionStopReason::AlreadyValid,
        };
    }

    let mut node = start;
    let mut skip = skip_seg;
    let mut distance_m = 0.0;
    let mut segments = Vec::new();
    let stopped;

    loop {
        let Some((next_node, next_seg, seg_len)) = next_hop(graph, node, skip) else {
            stopped = ExpansionStopReason::DeadEnd;
            break;
        };
        if distance_m + seg_len > max_leg_m {
            stopped = ExpansionStopReason::BudgetExhausted;
            break;
        }
        if let Some(dev) = graph.turn_deviation_deg(skip, node, next_seg) {
            if dev > max_turn_deviation_deg {
                stopped = ExpansionStopReason::SharpTurn { deviation_deg: dev };
                break;
            }
        }
        distance_m += seg_len;
        node = next_node;
        skip = next_seg;
        segments.push(next_seg);
        if graph.is_valid_node(node) {
            stopped = ExpansionStopReason::ReachedValidNode;
            break;
        }
    }

    Expansion { node, distance_m, segments, stopped }
}

/// The one segment touching `node` other than `skip`, if any. At an invalid
/// node (a pass-through, per `Graph::is_valid_node`) there is by construction
/// exactly one such neighbor to continue the walk into.
fn next_hop(graph: &Graph, node: NodeId, skip: SegmentId) -> Option<(NodeId, SegmentId, f64)> {
    graph.topology_neighbors(node)
        .iter()
        .find(|(_, seg)| *seg != skip)
        .and_then(|(other_node, seg_id)| {
            graph.segments.get(seg_id).map(|seg| (*other_node, *seg_id, seg.length_m))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, NetworkSegment};

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
    fn already_valid_node_does_not_expand() {
        let mut g = Graph::new();
        // Node 1 has three distinct neighbors — a real branch, already valid.
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 100.0));
        g.add_segment(seg(3, 1, 3, 100.0));
        let exp = expand_to_valid_node(&g, NodeId(1), SegmentId(1), 15_000.0, 180.0);
        assert_eq!(exp.node, NodeId(1));
        assert_eq!(exp.distance_m, 0.0);
    }

    #[test]
    fn walks_past_one_pass_through_node() {
        // Location's boundary is at node 1 (invalid: pass-through). The real
        // junction is at node 2. Arrived via seg 1 (0->1); must walk seg 2
        // (1->2) to reach it.
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 50.0));
        g.add_segment(seg(3, 2, 3, 100.0));
        g.add_segment(seg(4, 2, 4, 100.0)); // makes node 2 a real 3-way branch
        let exp = expand_to_valid_node(&g, NodeId(1), SegmentId(1), 15_000.0, 180.0);
        assert_eq!(exp.node, NodeId(2));
        assert!((exp.distance_m - 50.0).abs() < 1e-9);
    }

    #[test]
    fn stops_at_dead_end_when_no_valid_node_reachable() {
        // 0 -> 1 -> 2, and node 2 is a true dead end (degree 1) — so node 2
        // itself counts as valid, and expansion from node 1 should reach it.
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 50.0));
        let exp = expand_to_valid_node(&g, NodeId(1), SegmentId(1), 15_000.0, 180.0);
        assert_eq!(exp.node, NodeId(2));
        assert!((exp.distance_m - 50.0).abs() < 1e-9);
    }

    #[test]
    fn respects_max_leg_cap() {
        let mut g = Graph::new();
        g.add_segment(seg(1, 0, 1, 100.0));
        g.add_segment(seg(2, 1, 2, 50.0));
        g.add_segment(seg(3, 2, 3, 100.0));
        g.add_segment(seg(4, 2, 4, 100.0)); // node 2 would be valid, but out of budget
        let exp = expand_to_valid_node(&g, NodeId(1), SegmentId(1), 10.0, 180.0); // cap way below 50m
        assert_eq!(exp.node, NodeId(1), "should stay put rather than exceed the cap");
        assert_eq!(exp.distance_m, 0.0);
    }

    #[test]
    fn stops_before_an_unnavigable_turn_at_a_pass_through_node() {
        // 0 -> 1 (heading due east) -> 2 (doubling straight back west from 1,
        // a literal reversal) -> node 2 is a real 3-way branch (valid), but
        // the only way to reach it from node 1 requires a 180° turn. With a
        // sub-180 cap, expansion must stop at node 1 (still invalid) rather
        // than walk through a turn `sweep_coverage` would reject anyway.
        let mut g = Graph::new();
        g.add_segment(NetworkSegment {
            id: SegmentId(1), start_node: NodeId(0), end_node: NodeId(1),
            geometry: vec![(0.0, 0.0), (0.002, 0.0)], length_m: 100.0,
            frc: 3, fow: 3, direction: Direction::Both, stable_id: String::new(),
        });
        g.add_segment(NetworkSegment {
            id: SegmentId(2), start_node: NodeId(1), end_node: NodeId(2),
            geometry: vec![(0.002, 0.0), (0.0005, 0.0)], length_m: 50.0,
            frc: 3, fow: 3, direction: Direction::Both, stable_id: String::new(),
        });
        g.add_segment(seg(3, 2, 3, 100.0)); // makes node 2 a real 3-way branch
        g.add_segment(seg(4, 2, 4, 100.0));

        let permissive = expand_to_valid_node(&g, NodeId(1), SegmentId(1), 15_000.0, 180.0);
        assert_eq!(permissive.node, NodeId(2), "an unrestricted cap should walk through to the valid node");
        assert!((permissive.distance_m - 50.0).abs() < 1e-9);

        let strict = expand_to_valid_node(&g, NodeId(1), SegmentId(1), 15_000.0, 150.0);
        assert_eq!(strict.node, NodeId(1), "a 150° cap should refuse the 180° reversal and stay put");
        assert_eq!(strict.distance_m, 0.0);
        assert!(strict.segments.is_empty());
    }
}
