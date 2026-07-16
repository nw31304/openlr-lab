//! On-demand connectivity diagnostics for the encoder's waypoint-connecting
//! and coverage-sweep A* searches (`route_waypoints` / `sweep_coverage`,
//! both built on `openlr_graph::shortest_path`). Deliberately not a passive
//! trace: nothing here runs during a normal encode. A caller (the LLM tool
//! layer, via the wasm bindings) invokes `diagnose_connection` only *after*
//! a route search has already failed, spending an extra A* run to turn a
//! bare "no route" string into a precise, structured answer.

use openlr_graph::{shortest_path, Graph, NodeId, PathOutcome, SegmentId, TileKey};

/// A node along the unrestricted path where the turn deviation exceeds the
/// cap being diagnosed against.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SharpTurnPoint {
    pub node: NodeId,
    pub from_segment: SegmentId,
    pub to_segment: SegmentId,
    pub deviation_deg: f64,
}

/// Result of probing whether `from_node`/`to_node` are connected — twice:
/// once under the caller's actual `max_turn_deviation_deg`, and once fully
/// unrestricted (180°) — to distinguish "genuinely disconnected or wrong
/// direction" (fails both) from "connected, but only via a turn sharper than
/// the cap allows" (fails capped, succeeds unrestricted). In the latter
/// case, `sharp_turns` pinpoints exactly which node(s) on the unrestricted
/// path exceed the cap, so the caller doesn't have to walk the path itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConnectionDiagnosis {
    pub connected_within_cap: bool,
    pub length_within_cap_m: Option<f64>,
    pub connected_unrestricted: bool,
    pub length_unrestricted_m: Option<f64>,
    /// True when the unrestricted search succeeds but the capped one
    /// doesn't — the turn-angle gate, not connectivity or direction, is the
    /// actual blocker.
    pub blocked_by_turn_angle: bool,
    /// Populated only when `blocked_by_turn_angle` is true.
    pub sharp_turns: Vec<SharpTurnPoint>,
    /// Set when either search hit an unloaded tile boundary instead of a
    /// genuine pass/fail — load this tile and re-diagnose.
    pub needs_tile: Option<TileKey>,
}

/// See module doc comment. `from_seg` seeds the search the same way
/// `shortest_path` always does — pass `openlr_graph::NO_PRIOR_SEG` if there's
/// no real incoming edge to bias against (e.g. diagnosing from a bare
/// waypoint rather than mid-route).
pub fn diagnose_connection(
    graph: &Graph,
    from_node: NodeId,
    from_seg: SegmentId,
    to_node: NodeId,
    max_turn_deviation_deg: f64,
    zoom: u8,
) -> ConnectionDiagnosis {
    let empty = || ConnectionDiagnosis {
        connected_within_cap: false, length_within_cap_m: None,
        connected_unrestricted: false, length_unrestricted_m: None,
        blocked_by_turn_angle: false, sharp_turns: Vec::new(), needs_tile: None,
    };

    match shortest_path(graph, from_node, from_seg, to_node, 7, max_turn_deviation_deg, 0, zoom) {
        PathOutcome::Found(r) => ConnectionDiagnosis {
            connected_within_cap: true, length_within_cap_m: Some(r.length_m),
            connected_unrestricted: true, length_unrestricted_m: Some(r.length_m),
            ..empty()
        },
        PathOutcome::NeedsTile(tk) => ConnectionDiagnosis { needs_tile: Some(tk), ..empty() },
        PathOutcome::NoPath => {
            // Capped search failed — try again fully unrestricted to see
            // whether the turn-angle gate specifically is what's blocking.
            match shortest_path(graph, from_node, from_seg, to_node, 7, 180.0, 0, zoom) {
                PathOutcome::Found(r) => {
                    let sharp_turns = find_sharp_turns(graph, from_node, from_seg, &r.segments, max_turn_deviation_deg);
                    ConnectionDiagnosis {
                        connected_unrestricted: true,
                        length_unrestricted_m: Some(r.length_m),
                        blocked_by_turn_angle: !sharp_turns.is_empty(),
                        sharp_turns,
                        ..empty()
                    }
                }
                PathOutcome::NeedsTile(tk) => ConnectionDiagnosis { needs_tile: Some(tk), ..empty() },
                PathOutcome::NoPath => empty(),
            }
        }
    }
}

/// Walk `path` as actually traversed from `(start_node` via `start_seg)` and
/// report every node where the turn from the incoming to the outgoing
/// segment exceeds `max_turn_deviation_deg`.
fn find_sharp_turns(
    graph: &Graph,
    start_node: NodeId,
    start_seg: SegmentId,
    path: &[SegmentId],
    max_turn_deviation_deg: f64,
) -> Vec<SharpTurnPoint> {
    let mut out = Vec::new();
    let mut node = start_node;
    let mut prev_seg = start_seg;
    for &seg_id in path {
        if let Some(dev) = graph.turn_deviation_deg(prev_seg, node, seg_id) {
            if dev > max_turn_deviation_deg {
                out.push(SharpTurnPoint { node, from_segment: prev_seg, to_segment: seg_id, deviation_deg: dev });
            }
        }
        let Some(seg) = graph.segments.get(&seg_id) else { break };
        node = if seg.start_node == node { seg.end_node } else { seg.start_node };
        prev_seg = seg_id;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, NetworkNode, NetworkSegment, NO_PRIOR_SEG};

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: String::new(), is_boundary: false }
    }
    fn seg(id: u32, s: u32, e: u32, geom: Vec<(f64, f64)>) -> NetworkSegment {
        NetworkSegment {
            id: SegmentId(id), start_node: NodeId(s), end_node: NodeId(e),
            geometry: geom, length_m: 100.0, frc: 3, fow: 3,
            direction: Direction::Both, stable_id: String::new(),
        }
    }

    #[test]
    fn connected_within_cap_reports_both_true() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_segment(seg(1, 0, 1, vec![(0.0, 0.0), (0.001, 0.0)]));
        let d = diagnose_connection(&g, NodeId(0), NO_PRIOR_SEG, NodeId(1), 180.0, 12);
        assert!(d.connected_within_cap);
        assert!(d.connected_unrestricted);
        assert!(!d.blocked_by_turn_angle);
    }

    #[test]
    fn genuinely_disconnected_reports_both_false() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.1, 0.1));
        let d = diagnose_connection(&g, NodeId(0), NO_PRIOR_SEG, NodeId(1), 180.0, 12);
        assert!(!d.connected_within_cap);
        assert!(!d.connected_unrestricted);
        assert!(!d.blocked_by_turn_angle);
        assert!(d.sharp_turns.is_empty());
    }

    #[test]
    fn turn_angle_blocked_pinpoints_the_sharp_node() {
        // 0 -> via (heading east) -> 2, where the via->2 hop is a near-reversal
        // (165° deviation) — connected unrestricted, but not under a 150° cap.
        let mut g = Graph::new();
        g.add_node(node(0, 0.000, 0.000));
        g.add_node(node(2, 0.003, 0.000));
        g.add_node(node(3, 0.0001022, -0.0007765));
        g.add_segment(seg(1, 0, 2, vec![(0.000, 0.000), (0.003, 0.000)]));
        g.add_segment(seg(2, 2, 3, vec![(0.003, 0.000), (0.0001022, -0.0007765)]));

        let capped = diagnose_connection(&g, NodeId(0), NO_PRIOR_SEG, NodeId(3), 150.0, 12);
        assert!(!capped.connected_within_cap);
        assert!(capped.connected_unrestricted);
        assert!(capped.blocked_by_turn_angle);
        assert_eq!(capped.sharp_turns.len(), 1);
        assert_eq!(capped.sharp_turns[0].node, NodeId(2));
        assert!(capped.sharp_turns[0].deviation_deg > 150.0);

        let unrestricted = diagnose_connection(&g, NodeId(0), NO_PRIOR_SEG, NodeId(3), 180.0, 12);
        assert!(unrestricted.connected_within_cap);
    }
}
