use std::collections::{HashMap, HashSet};
use crate::{NetworkNode, NetworkSegment, NodeId, SegmentId, TurnRestriction, Direction, TileKey};
use crate::geometry::{bearing_at_offset, haversine_m, polyline_length_m, project_onto_polyline};

// Grid cell size: 1/500 degree ≈ 222 m at equator.  Fine enough to cover the
// default 30 m candidate radius in a 3×3 neighbourhood and the 200 m permissive
// radius in a ~5×5 one; coarse enough that most cells are empty and insertions
// cheap.  A segment is indexed into every cell its geometry bounding box touches.
const GRID_SCALE: f64 = 500.0;

/// Why a candidate successor edge was filtered out by `successors_skipped()`.
///
/// U-turns (`seg_id == incoming_seg`) are not reported here — they are an
/// unconditional routing rule, not a data-quality signal.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EdgeSkipReason {
    /// Segment FRC is numerically greater than LFRCNP (less important than the
    /// leg's lowest-FRC-to-next-point floor).
    FrcBelowLfrcnp { seg_frc: u8, lfrcnp: u8 },
    /// An explicit turn restriction prohibits `incoming_seg → node → this_seg`.
    TurnRestricted,
    /// The outgoing edge map contains this segment, but the segment's stored
    /// direction does not permit entry from `node`.  Should be rare in practice
    /// (indicates a graph-construction inconsistency), but surfaced here so it
    /// is never silently swallowed.
    DirectionBlocked,
    /// The geometric turn angle at `node` exceeds `max_turn_deviation_deg`: the
    /// route would double back on itself more sharply than configured.
    SharpTurn { deviation_deg: f64 },
}

/// Compass bearing (degrees) from `node`'s position out along `seg`, away from
/// the node — i.e. what you'd face standing at the junction looking into this
/// road, regardless of which end of the segment `node` is or which way the
/// segment is traversed. Used to measure the geometric angle between two roads
/// meeting at a node (the turn-angle gate), and doubles as the OpenLR BEAR
/// attribute computation for encoding: for a normal LRP this is "look into the
/// outgoing leg" (forward-facing); for the last LRP of a location this is
/// "look into the incoming leg, away from arrival" — which is exactly the
/// whitepaper's reversed bearing convention for the last LRP (§5.2.4), with no
/// special-casing needed since "away from node" already flips appropriately.
/// Returns `None` if `node` is not an endpoint of `seg` or the geometry is
/// degenerate.
pub fn bearing_away_from_node(seg: &NetworkSegment, node: NodeId) -> Option<f64> {
    if seg.geometry.len() < 2 {
        return None;
    }
    if seg.start_node == node {
        Some(bearing_at_offset(&seg.geometry, 0.0, true))
    } else if seg.end_node == node {
        Some(bearing_at_offset(&seg.geometry, polyline_length_m(&seg.geometry), false))
    } else {
        None
    }
}

/// In-memory routing graph built from loaded tile data.
///
/// `outgoing[node]` lists every segment that can be *entered* from `node`:
///   - `Direction::Forward | Both` → reachable from `start_node`
///   - `Direction::Backward | Both` → reachable from `end_node`
pub struct Graph {
    pub segments: HashMap<SegmentId, NetworkSegment>,
    pub nodes: HashMap<NodeId, NetworkNode>,
    outgoing: HashMap<NodeId, Vec<SegmentId>>,
    /// Undirected topology: node → every (other_end_node, segment_id) touching it,
    /// ignoring `Direction` entirely. Unlike `outgoing` (which reflects only what a
    /// router may traverse), this is the raw real-world shape of the network —
    /// needed for `is_valid_node`, which must see actual junction geometry, not
    /// the direction-filtered routing view.
    topology: HashMap<NodeId, Vec<(NodeId, SegmentId)>>,
    restrictions: Vec<TurnRestriction>,
    /// Spatial grid: grid-cell key → segment IDs whose geometry bbox overlaps that cell.
    /// Cell size ≈ 222 m; turns `segments_near` from O(total) to O(local density).
    spatial_grid: HashMap<(i32, i32), Vec<SegmentId>>,
    /// Tiles that have been injected into this graph (including empty tiles marked as
    /// loaded so A* does not repeatedly request them).
    loaded_tiles: HashSet<TileKey>,
}

impl Default for Graph {
    fn default() -> Self { Self::new() }
}

impl Graph {
    pub fn new() -> Self {
        Self {
            segments: HashMap::new(),
            nodes: HashMap::new(),
            outgoing: HashMap::new(),
            topology: HashMap::new(),
            restrictions: Vec::new(),
            spatial_grid: HashMap::new(),
            loaded_tiles: HashSet::new(),
        }
    }

    /// Record that tile `(z, x, y)` has been loaded into this graph.
    /// Also called for tiles that are absent from the archive (empty) so that
    /// A* does not keep requesting them — boundary nodes in those tiles are
    /// treated as genuine dead ends.
    pub fn mark_tile_loaded(&mut self, z: u8, x: u32, y: u32) {
        self.loaded_tiles.insert(TileKey { z, x, y });
    }

    /// Return true if `tk` has been loaded (or marked as empty) into this graph.
    pub fn is_tile_loaded(&self, tk: TileKey) -> bool {
        self.loaded_tiles.contains(&tk)
    }

    pub fn add_segment(&mut self, seg: NetworkSegment) {
        let id    = seg.id;
        let start = seg.start_node;
        let end   = seg.end_node;

        self.topology.entry(start).or_default().push((end, id));
        if end != start {
            self.topology.entry(end).or_default().push((start, id));
        }

        match seg.direction {
            Direction::Forward | Direction::Both => {
                self.outgoing.entry(start).or_default().push(id);
            }
            Direction::Backward => {}
        }
        match seg.direction {
            Direction::Backward | Direction::Both => {
                self.outgoing.entry(end).or_default().push(id);
            }
            Direction::Forward => {}
        }

        // Index every grid cell the segment's geometry bounding box overlaps.
        if seg.geometry.len() >= 2 {
            let (mut lon_min, mut lon_max) = (f64::MAX, f64::MIN);
            let (mut lat_min, mut lat_max) = (f64::MAX, f64::MIN);
            for &(lon, lat) in &seg.geometry {
                if lon < lon_min { lon_min = lon; }
                if lon > lon_max { lon_max = lon; }
                if lat < lat_min { lat_min = lat; }
                if lat > lat_max { lat_max = lat; }
            }
            let cx0 = (lon_min * GRID_SCALE).floor() as i32;
            let cx1 = (lon_max * GRID_SCALE).floor() as i32;
            let cy0 = (lat_min * GRID_SCALE).floor() as i32;
            let cy1 = (lat_max * GRID_SCALE).floor() as i32;
            for cx in cx0..=cx1 {
                for cy in cy0..=cy1 {
                    self.spatial_grid.entry((cx, cy)).or_default().push(id);
                }
            }
        }

        self.segments.insert(id, seg);
    }

    pub fn add_node(&mut self, node: NetworkNode) {
        self.nodes.insert(node.id, node);
    }

    pub fn add_restriction(&mut self, r: TurnRestriction) {
        self.restrictions.push(r);
    }

    pub fn restrictions_count(&self) -> usize {
        self.restrictions.len()
    }

    pub fn restrictions(&self) -> &[TurnRestriction] {
        &self.restrictions
    }

    /// Segments within `radius_m` of `(lon, lat)`. Returns `(segment_id, distance_m)`.
    ///
    /// Uses the spatial grid to visit only cells near the query point, keeping the
    /// work proportional to the local road density rather than the total graph size.
    pub fn segments_near(&self, lon: f64, lat: f64, radius_m: f64) -> Vec<(SegmentId, f64)> {
        // 1° latitude ≈ 111 km; add one full cell (1/GRID_SCALE°) as buffer so a
        // segment whose geometry bounding box clips the edge of the query circle is
        // not missed by the grid lookup before `project_onto_polyline` does the
        // exact check.
        let buf_deg = radius_m / 111_000.0 + 1.0 / GRID_SCALE;
        let cx0 = ((lon - buf_deg) * GRID_SCALE).floor() as i32;
        let cx1 = ((lon + buf_deg) * GRID_SCALE).floor() as i32;
        let cy0 = ((lat - buf_deg) * GRID_SCALE).floor() as i32;
        let cy1 = ((lat + buf_deg) * GRID_SCALE).floor() as i32;

        let mut seen: HashSet<SegmentId> = HashSet::new();
        let mut result: Vec<(SegmentId, f64)> = Vec::new();

        for cx in cx0..=cx1 {
            for cy in cy0..=cy1 {
                if let Some(ids) = self.spatial_grid.get(&(cx, cy)) {
                    for &seg_id in ids {
                        if !seen.insert(seg_id) { continue; }
                        if let Some(seg) = self.segments.get(&seg_id) {
                            if let Some(proj) = project_onto_polyline(lon, lat, &seg.geometry) {
                                if proj.distance_m <= radius_m {
                                    result.push((seg_id, proj.distance_m));
                                }
                            }
                        }
                    }
                }
            }
        }

        result
    }

    /// Is the turn `from_seg → via_node → to_seg` explicitly restricted?
    pub fn is_restricted(&self, from_seg: SegmentId, via_node: NodeId, to_seg: SegmentId) -> bool {
        self.restrictions.iter().any(|r| {
            r.from_seg == from_seg && r.via_node == via_node && r.to_seg == to_seg
        })
    }

    /// OpenLR Rule-4 (whitepaper §6, Figures 17-18): is `node` a genuine routing
    /// decision point, or can a shortest-path search safely step over it? Encoders
    /// must anchor LRPs on valid nodes wherever possible, because real junctions
    /// are far more likely to be represented consistently across different map
    /// datasets than an arbitrary digitized shape vertex.
    ///
    /// - 0 or 1 distinct neighbors → a genuine dead end. Nothing to pass through,
    ///   so it counts as valid (this case isn't a "pass-through" at all).
    /// - Exactly 2 distinct neighbors → a simple corridor (one-way per Figure 17,
    ///   or two-way per Figure 18) — invalid unless a U-turn is possible there.
    /// - 3+ distinct neighbors → valid. (v1 does not implement openlr-tt's further
    ///   generalization that a higher-degree node can *still* be invalid if every
    ///   neighbor pairs up into a collinear through-route, e.g. two roads merely
    ///   crossing with no real interchange. This is a deliberately conservative
    ///   simplification: it can only make us treat a node as valid when the full
    ///   spec would not, which costs at most one fewer expansion hop — never a
    ///   correctness bug, since the "no valid node reachable" escape hatch already
    ///   handles the opposite gap.)
    pub fn is_valid_node(&self, node: NodeId) -> bool {
        let touching = match self.topology.get(&node) {
            Some(t) => t,
            None => return true,
        };
        let distinct: HashSet<NodeId> = touching.iter().map(|(other, _)| *other).collect();
        match distinct.len() {
            0 | 1 => true,
            2 => self.uturn_possible_at(touching),
            _ => true,
        }
    }

    /// Every segment touching `node`, from the raw undirected topology (ignoring
    /// `Direction`) — the real-world network shape. Used by encoding's
    /// valid-node expansion (walking outward to find a robust LRP anchor), as
    /// opposed to `successors()`'s FRC/direction-filtered routing view.
    pub fn topology_neighbors(&self, node: NodeId) -> &[(NodeId, SegmentId)] {
        self.topology.get(&node).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Segments that can actually be *departed* from `node` in their permitted
    /// travel direction (`node == seg.start_node` for `Forward`/`Both`, or
    /// `node == seg.end_node` for `Backward`/`Both`) — unlike
    /// `topology_neighbors`, which ignores `Direction` entirely. Needed
    /// wherever a caller anchors a *travel direction* at this node with
    /// nothing downstream to catch a wrong-direction pick the way A* routing
    /// would (e.g. snapping a PointAlongLine point directly onto a node: its
    /// line *is* the path, verbatim, with no coverage-sweep search to reject
    /// an illegal direction afterward).
    pub fn outgoing_segments(&self, node: NodeId) -> &[SegmentId] {
        self.outgoing.get(&node).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// v1 approximation of "a U-turn is possible at this node": true iff there are
    /// multiple distinct segments connecting it to the *same* neighbor (e.g. two
    /// parallel one-way carriageways forming a real turnaround point). Does not
    /// model exotic single-segment U-turn geometries — again, the safe direction,
    /// since the expansion escape hatch covers whatever this misses.
    fn uturn_possible_at(&self, touching: &[(NodeId, SegmentId)]) -> bool {
        let mut counts: HashMap<NodeId, u32> = HashMap::new();
        for (other, _seg) in touching {
            *counts.entry(*other).or_insert(0) += 1;
        }
        counts.values().any(|&c| c > 1)
    }

    /// Angular deviation from a straight-through continuation at `node`, in
    /// degrees, `[0, 180]`. `0` = the route continues straight ahead; `180` =
    /// the route doubles back on itself (a full U-turn). `None` if either
    /// segment's geometry can't be read at `node` (missing/degenerate data —
    /// callers should treat this permissively, not as a rejection).
    ///
    /// `pub` (not just used internally by `successors`/`successors_skipped`)
    /// because the same check is needed for transitions `successors()` never
    /// sees: an LRP's own boundary segment — the "partial edge" spanning from
    /// where a candidate sits to the node it hands off to the next leg at —
    /// is an arithmetic distance computation, not an A*-searched edge, so it
    /// never passes through `successors()`'s turn-angle gate at all. Decode's
    /// candidate-combination logic (`openlr-engine`) calls this directly to
    /// close that gap.
    pub fn turn_deviation_deg(&self, from_seg: SegmentId, node: NodeId, to_seg: SegmentId) -> Option<f64> {
        let from = self.segments.get(&from_seg)?;
        let to = self.segments.get(&to_seg)?;
        let b_in = bearing_away_from_node(from, node)?;
        let b_out = bearing_away_from_node(to, node)?;
        let mut diff = (b_in - b_out).abs() % 360.0;
        if diff > 180.0 {
            diff = 360.0 - diff;
        }
        Some(180.0 - diff)
    }

    /// Successor edges reachable from `(node, incoming_seg)`.
    ///
    /// Returns `(next_node, seg_id, length_m)` for every edge that:
    /// - can be entered from `node` in the direction it permits,
    /// - has `seg.frc ≤ lfrcnp` (LFRCNP floor — Invariant 9),
    /// - is not blocked by an explicit turn restriction,
    /// - does not double back more sharply than `max_turn_deviation_deg` allows
    ///   (set to `180.0` to disable this gate).
    pub fn successors(
        &self,
        node: NodeId,
        incoming_seg: SegmentId,
        lfrcnp: u8,
        max_turn_deviation_deg: f64,
    ) -> Vec<(NodeId, SegmentId, f64)> {
        let mut result = Vec::new();
        for &seg_id in self.outgoing.get(&node).into_iter().flatten() {
            // U-turns are illegal: never re-enter the segment we just departed from.
            if seg_id == incoming_seg {
                continue;
            }
            let seg = match self.segments.get(&seg_id) {
                Some(s) => s,
                None => continue,
            };
            // FRC floor: only use roads at or above LFRCNP importance
            if seg.frc > lfrcnp {
                continue;
            }
            // Turn restriction
            if self.is_restricted(incoming_seg, node, seg_id) {
                continue;
            }
            // Determine the next node and verify the direction is traversable.
            let next_node = match seg.direction {
                Direction::Forward | Direction::Both if seg.start_node == node => seg.end_node,
                Direction::Backward | Direction::Both if seg.end_node == node   => seg.start_node,
                _ => continue,
            };
            // Turn-angle gate: reject candidates that double back beyond the configured limit.
            if max_turn_deviation_deg < 180.0 {
                if let Some(dev) = self.turn_deviation_deg(incoming_seg, node, seg_id) {
                    if dev > max_turn_deviation_deg {
                        continue;
                    }
                }
            }
            result.push((next_node, seg_id, seg.length_m));
        }
        result
    }

    /// Returns the outgoing edges from `(node, incoming_seg)` that were **filtered out**
    /// by `successors()`, with the reason each was skipped.
    ///
    /// Mirrors the filter chain in `successors()` exactly.  Called at Summary+ trace
    /// level for aggregate skip counting and at Full level for per-edge events.
    pub fn successors_skipped(
        &self,
        node: NodeId,
        incoming_seg: SegmentId,
        lfrcnp: u8,
        max_turn_deviation_deg: f64,
    ) -> Vec<(SegmentId, EdgeSkipReason)> {
        let mut skipped = Vec::new();
        for &seg_id in self.outgoing.get(&node).into_iter().flatten() {
            if seg_id == incoming_seg { continue; } // U-turn — expected, not a skip to report
            let seg = match self.segments.get(&seg_id) {
                Some(s) => s,
                None => continue,
            };
            if seg.frc > lfrcnp {
                skipped.push((seg_id, EdgeSkipReason::FrcBelowLfrcnp { seg_frc: seg.frc, lfrcnp }));
                continue;
            }
            if self.is_restricted(incoming_seg, node, seg_id) {
                skipped.push((seg_id, EdgeSkipReason::TurnRestricted));
                continue;
            }
            let ok = match seg.direction {
                Direction::Forward | Direction::Both if seg.start_node == node => true,
                Direction::Backward | Direction::Both if seg.end_node == node  => true,
                _ => false,
            };
            if !ok {
                skipped.push((seg_id, EdgeSkipReason::DirectionBlocked));
                continue;
            }
            if max_turn_deviation_deg < 180.0 {
                if let Some(dev) = self.turn_deviation_deg(incoming_seg, node, seg_id) {
                    if dev > max_turn_deviation_deg {
                        skipped.push((seg_id, EdgeSkipReason::SharpTurn { deviation_deg: dev }));
                    }
                }
            }
        }
        skipped
    }

    /// Haversine distance from node to (lon, lat). Returns None if node not loaded.
    pub fn node_dist_m(&self, node: NodeId, lon: f64, lat: f64) -> Option<f64> {
        self.nodes.get(&node).map(|n| haversine_m(n.lon, n.lat, lon, lat))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Direction;

    fn make_seg(id: u32, start: u32, end: u32, frc: u8, dir: Direction) -> NetworkSegment {
        NetworkSegment {
            id: SegmentId(id),
            start_node: NodeId(start),
            end_node: NodeId(end),
            geometry: vec![(0.0, 0.0), (0.001, 0.001)],
            length_m: 100.0,
            frc,
            fow: 3,
            direction: dir,
            stable_id: String::new(),
        }
    }

    #[test]
    fn successors_respects_lfrcnp() {
        let mut g = Graph::new();
        g.add_segment(make_seg(1, 0, 1, 2, Direction::Both));
        g.add_segment(make_seg(2, 1, 2, 4, Direction::Both)); // FRC=4
        g.add_segment(make_seg(3, 1, 3, 3, Direction::Both)); // FRC=3

        // From node 1 via seg 1, LFRCNP=3 → seg 2 (FRC 4 > 3) should be excluded
        let succs = g.successors(NodeId(1), SegmentId(1), 3, 180.0);
        let ids: Vec<_> = succs.iter().map(|s| s.1.0).collect();
        assert!(ids.contains(&3), "seg 3 should be included");
        assert!(!ids.contains(&2), "seg 2 FRC=4 exceeds lfrcnp=3");
    }

    /// Segment 1 runs west→east into node 1. Segment 2 continues straight east.
    /// Segment 3 folds back to the southwest — from node 1 looking back along
    /// seg 1 you face west; continuing onto seg 3 would send you almost the
    /// same way you came, i.e. close to a U-turn.
    #[test]
    fn successors_turn_angle_gate() {
        let mut g = Graph::new();
        let mut seg1 = make_seg(1, 0, 1, 3, Direction::Both);
        seg1.geometry = vec![(0.0, 0.0), (0.01, 0.0)]; // west -> east, arrives at node 1 heading east
        let mut seg2 = make_seg(2, 1, 2, 3, Direction::Both);
        seg2.geometry = vec![(0.01, 0.0), (0.02, 0.0)]; // continues straight east
        let mut seg3 = make_seg(3, 1, 3, 3, Direction::Both);
        seg3.geometry = vec![(0.01, 0.0), (0.0, -0.0001)]; // folds back almost due west
        g.add_segment(seg1);
        g.add_segment(seg2);
        g.add_segment(seg3);

        // Disabled (180.0): both the straight continuation and the near-U-turn pass.
        let succs = g.successors(NodeId(1), SegmentId(1), 3, 180.0);
        let ids: Vec<_> = succs.iter().map(|s| s.1.0).collect();
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));

        // Capped at 150°: the near-U-turn onto seg 3 is excluded, straight-ahead seg 2 survives.
        let succs = g.successors(NodeId(1), SegmentId(1), 3, 150.0);
        let ids: Vec<_> = succs.iter().map(|s| s.1.0).collect();
        assert!(ids.contains(&2), "straight continuation should survive a 150° cap");
        assert!(!ids.contains(&3), "near-U-turn should be excluded by a 150° cap");

        let skipped = g.successors_skipped(NodeId(1), SegmentId(1), 3, 150.0);
        assert!(skipped.iter().any(|(id, reason)| {
            *id == SegmentId(3) && matches!(reason, EdgeSkipReason::SharpTurn { deviation_deg } if *deviation_deg > 150.0)
        }));
    }

    #[test]
    fn dead_end_is_valid() {
        let mut g = Graph::new();
        g.add_segment(make_seg(1, 0, 1, 3, Direction::Both));
        assert!(g.is_valid_node(NodeId(1)), "node 1 has only one neighbor — a dead end");
    }

    /// Regression: `snap_point`'s node-hint path (openlr-wasm) used to pick
    /// from `topology_neighbors`, which ignores `Direction` — anchoring a
    /// PointAlongLine reference on a segment in the *prohibited* direction
    /// when a one-way road happened to be first in the adjacency list, with
    /// nothing downstream to catch it (PAL has no A*/coverage-sweep step).
    /// `outgoing_segments` must exclude a segment that can't actually be
    /// departed from this node, even though it's a real topological neighbor.
    #[test]
    fn outgoing_segments_excludes_wrong_direction_departures() {
        let mut g = Graph::new();
        // seg 1: node 0 -> node 1, one-way Forward — cannot be departed *from*
        // node 1 (only entered there), even though it's a real topology neighbor.
        g.add_segment(make_seg(1, 0, 1, 3, Direction::Forward));
        // seg 2: node 1 -> node 2, one-way Forward — can be departed from node 1.
        g.add_segment(make_seg(2, 1, 2, 3, Direction::Forward));

        let topo: Vec<_> = g.topology_neighbors(NodeId(1)).iter().map(|(_, s)| s.0).collect();
        assert!(topo.contains(&1) && topo.contains(&2), "both are real topology neighbors of node 1");

        let outgoing: Vec<_> = g.outgoing_segments(NodeId(1)).iter().map(|s| s.0).collect();
        assert!(!outgoing.contains(&1), "seg 1 can only be entered at node 1, not departed — must be excluded");
        assert!(outgoing.contains(&2), "seg 2 can be departed from node 1 — must be included");
    }

    #[test]
    fn one_way_pass_through_is_invalid() {
        // 0 -> 1 -> 2, strictly one-way: node 1 has exactly one viable through-path.
        let mut g = Graph::new();
        g.add_segment(make_seg(1, 0, 1, 3, Direction::Forward));
        g.add_segment(make_seg(2, 1, 2, 3, Direction::Forward));
        assert!(!g.is_valid_node(NodeId(1)), "one-way corridor (Figure 17) should be invalid");
    }

    #[test]
    fn two_way_pass_through_is_invalid() {
        // 0 <-> 1 <-> 2, bidirectional, no other branch, no parallel segments.
        let mut g = Graph::new();
        g.add_segment(make_seg(1, 0, 1, 3, Direction::Both));
        g.add_segment(make_seg(2, 1, 2, 3, Direction::Both));
        assert!(!g.is_valid_node(NodeId(1)), "two-way corridor (Figure 18) should be invalid absent a U-turn");
    }

    #[test]
    fn parallel_segments_to_same_neighbor_allow_uturn() {
        // Node 1 touches two distinct neighbors (0 and 2), which would normally
        // make it an invalid 2-neighbor pass-through — but there are two distinct
        // one-way segments both connecting it to neighbor 2 (e.g. a divided
        // carriageway), which is this v1's approximation of "a U-turn is possible
        // here", so node 1 counts as valid.
        let mut g = Graph::new();
        g.add_segment(make_seg(1, 0, 1, 3, Direction::Both));
        g.add_segment(make_seg(2, 1, 2, 3, Direction::Forward));
        g.add_segment(make_seg(3, 2, 1, 3, Direction::Forward));
        assert!(g.is_valid_node(NodeId(1)), "parallel segments to the same neighbor should allow a U-turn");
    }

    #[test]
    fn real_branch_is_valid() {
        // Node 1 connects to three distinct neighbors — a genuine junction.
        let mut g = Graph::new();
        g.add_segment(make_seg(1, 0, 1, 3, Direction::Both));
        g.add_segment(make_seg(2, 1, 2, 3, Direction::Both));
        g.add_segment(make_seg(3, 1, 3, 3, Direction::Both));
        assert!(g.is_valid_node(NodeId(1)), "a real 3-way branch should be valid");
    }

    #[test]
    fn segments_near_filters_by_radius() {
        let mut g = Graph::new();
        let mut far = make_seg(10, 0, 1, 2, Direction::Both);
        far.geometry = vec![(10.0, 10.0), (10.001, 10.001)]; // far away
        g.add_segment(make_seg(1, 0, 1, 2, Direction::Both));
        g.add_segment(far);
        let near = g.segments_near(0.0005, 0.0005, 200.0);
        assert_eq!(near.len(), 1);
        assert_eq!(near[0].0, SegmentId(1));
    }
}
