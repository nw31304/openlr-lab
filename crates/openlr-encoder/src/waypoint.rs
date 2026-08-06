//! Turning a raw map click (or a batch of them) into a graph anchor, and
//! chaining shortest-path routing between consecutive anchors. This is real
//! encoding logic — CLAUDE.md Invariant 10 applies here directly (`snap_point`
//! anchors a *travel direction* with no A*/routing step afterward to catch a
//! wrong-direction pick, unlike interior A* routing) — moved here from
//! `openlr-wasm` (where it originated as wasm-bindgen glue that happened to
//! also contain the only implementation) so a native caller doesn't have to
//! duplicate it or go through JSON/wasm to get waypoint-based encoding.

use std::collections::HashSet;

use openlr_graph::{
    project_onto_polyline, shortest_path, Direction, Graph, NodeId, PathOutcome, PathResult,
    SegmentId, TileKey, NO_PRIOR_SEG,
};

/// Default snap radius for resolving a raw click to a nearby road or
/// intersection. Not (yet) a caller-configurable parameter — every call site
/// in this crate and in `openlr-wasm` uses this same value today.
pub const WAYPOINT_SNAP_RADIUS_M: f64 = 50.0;

/// One raw waypoint: a click position plus an optional explicit
/// disambiguation choice (from [`nearby_anchors`]) for when the click landed
/// near more than one plausible road or intersection.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct Waypoint {
    pub lon: f64,
    pub lat: f64,
    /// Explicit disambiguation choice from `nearby_anchors` — when the click
    /// was near multiple plausible roads, snap onto exactly this segment
    /// instead of silently picking the nearest one. Ignored if `node_id` is
    /// also set (node wins — see `SnapHint`).
    #[serde(default)]
    pub segment_id: Option<u32>,
    /// Explicit disambiguation choice: snap directly to this intersection
    /// node instead of to a point along some road, regardless of which
    /// segment is geometrically nearest.
    #[serde(default)]
    pub node_id: Option<u32>,
}

/// The three ways a waypoint can resolve to a place on the graph.
pub enum SnapHint {
    /// Snap directly to this node (an explicit "this intersection" choice) —
    /// offset is always zero, since the user picked the junction itself, not
    /// a position along a particular road.
    Node(NodeId),
    /// Snap onto this specific segment (an explicit "this road" choice),
    /// still choosing whichever endpoint is nearer and computing a real
    /// offset — same as the default, just without the nearest-segment search.
    Segment(SegmentId),
    /// No explicit choice — pick whichever nearby segment is geometrically
    /// nearest (today's default, unambiguous-click behavior).
    Nearest,
}

impl Waypoint {
    pub fn snap_hint(&self) -> SnapHint {
        if let Some(id) = self.node_id { SnapHint::Node(NodeId(id)) }
        else if let Some(id) = self.segment_id { SnapHint::Segment(SegmentId(id)) }
        else { SnapHint::Nearest }
    }
}

/// A candidate anchor for the *first or last* waypoint of a route — the two
/// boundary LRPs, which alone carry a POFF/NOFF offset in the Line format.
///
/// Unlike an interior waypoint (nearest-endpoint snapping is fine, since no
/// offset is ever recorded for it), a boundary offset is only reconstructible
/// by the decoder if it's a *forward* distance from a node the recorded path
/// genuinely starts (or ends) at, through to the click. When the click lands
/// mid-segment, that forward-reachable node is not necessarily the nearer
/// endpoint — it depends on which endpoint the rest of the route actually
/// continues from, which isn't decidable from proximity alone (see
/// `resolve_boundary_leg`, which tries both and picks whichever connects).
pub struct BoundaryCandidate {
    pub seg_id: SegmentId,
    /// Node the offset is measured from — becomes the location's start/end
    /// anchor node if this candidate wins.
    pub anchor: NodeId,
    /// The opposite endpoint: where the rest of the route actually connects.
    /// Equal to `anchor` (offset always 0) when the click snapped exactly
    /// onto a node, or resolved to an existing node via `SnapHint::Node`.
    pub continuation: NodeId,
    /// Forward distance from `anchor`, through `seg_id`, to the click.
    pub offset_m: f64,
}

impl BoundaryCandidate {
    /// The segment to bias the onward search against re-entering — `seg_id`
    /// when this candidate actually walks through it to reach `continuation`,
    /// or `NO_PRIOR_SEG` when `anchor == continuation` (nothing was walked).
    fn bias_seg(&self) -> SegmentId {
        if self.anchor == self.continuation { NO_PRIOR_SEG } else { self.seg_id }
    }
}

/// Every way waypoint `w` could anchor a location boundary: a single
/// zero-offset candidate for an explicit node pick, or two — one per segment
/// endpoint — for a segment/nearest-road snap, since which endpoint the path
/// actually continues from can't be decided without knowing where the route
/// goes next (the caller tries both — see `resolve_boundary_leg`).
pub fn boundary_candidates(graph: &Graph, w: &Waypoint) -> Option<Vec<BoundaryCandidate>> {
    if let SnapHint::Node(node_id) = w.snap_hint() {
        if graph.nodes.contains_key(&node_id) {
            let seg_id = graph.topology_neighbors(node_id).first().map(|(_, s)| *s)?;
            return Some(vec![BoundaryCandidate { seg_id, anchor: node_id, continuation: node_id, offset_m: 0.0 }]);
        }
        // Hinted node no longer loaded — fall through to nearest-segment search.
    }
    let seg_id = match w.snap_hint() {
        SnapHint::Segment(id) if graph.segments.contains_key(&id) => id,
        _ => {
            let nearby = graph.segments_near(w.lon, w.lat, WAYPOINT_SNAP_RADIUS_M);
            nearby.into_iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?.0
        }
    };
    let seg = graph.segments.get(&seg_id)?;
    let proj = project_onto_polyline(w.lon, w.lat, &seg.geometry)?;
    let total = seg.length_m;
    Some(vec![
        BoundaryCandidate { seg_id, anchor: seg.start_node, continuation: seg.end_node, offset_m: proj.arc_offset_m },
        BoundaryCandidate { seg_id, anchor: seg.end_node, continuation: seg.start_node, offset_m: total - proj.arc_offset_m },
    ])
}

/// Try every `candidates` entry's continuation against the fixed `target`
/// node on the other side of this leg — always an interior waypoint's
/// snapped node, since a route with no interior waypoints resolves both
/// boundaries jointly instead (see `route_waypoints`). Picks whichever
/// candidate yields the shortest total distance (its own offset plus the
/// connecting search). There's no incoming leg to bias against here — this
/// is always the very first leg of the route.
fn resolve_boundary_leg(
    graph: &Graph,
    candidates: &[BoundaryCandidate],
    target: NodeId,
    max_turn_deviation_deg: f64,
    zoom: u8,
) -> Result<(usize, PathResult), RouteOutcome> {
    let mut best: Option<(usize, PathResult, f64)> = None;
    for (idx, c) in candidates.iter().enumerate() {
        let (result, extra_len) = if c.continuation == target {
            (PathResult { segments: vec![], length_m: 0.0 }, 0.0)
        } else {
            match shortest_path(graph, c.continuation, c.bias_seg(), target, 7, max_turn_deviation_deg, 0, zoom) {
                PathOutcome::Found(r) => { let l = r.length_m; (r, l) }
                PathOutcome::NeedsTile(tk) => return Err(RouteOutcome::NeedsTile(tk)),
                PathOutcome::NoPath => continue,
            }
        };
        let total = c.offset_m + extra_len;
        if best.as_ref().map_or(true, |b| total < b.2) {
            best = Some((idx, result, total));
        }
    }
    best.map(|(idx, r, _)| (idx, r))
        .ok_or_else(|| RouteOutcome::Error(RouteFailure::plain("no route found for a boundary waypoint")))
}

/// Mirror of `resolve_boundary_leg` for the *last* boundary: the fixed node
/// is the source (the previous leg's arrival point) and the search runs
/// forward from it to each candidate's continuation.
fn resolve_boundary_leg_from(
    graph: &Graph,
    source: NodeId,
    source_bias_seg: SegmentId,
    candidates: &[BoundaryCandidate],
    max_turn_deviation_deg: f64,
    zoom: u8,
) -> Result<(usize, PathResult), RouteOutcome> {
    let mut best: Option<(usize, PathResult, f64)> = None;
    for (idx, c) in candidates.iter().enumerate() {
        let (result, extra_len) = if source == c.continuation {
            (PathResult { segments: vec![], length_m: 0.0 }, 0.0)
        } else {
            match shortest_path(graph, source, source_bias_seg, c.continuation, 7, max_turn_deviation_deg, 0, zoom) {
                PathOutcome::Found(r) => { let l = r.length_m; (r, l) }
                PathOutcome::NeedsTile(tk) => return Err(RouteOutcome::NeedsTile(tk)),
                PathOutcome::NoPath => continue,
            }
        };
        let total = c.offset_m + extra_len;
        if best.as_ref().map_or(true, |b| total < b.2) {
            best = Some((idx, result, total));
        }
    }
    best.map(|(idx, r, _)| (idx, r))
        .ok_or_else(|| RouteOutcome::Error(RouteFailure::plain("no route found for a boundary waypoint")))
}

/// A waypoint snapped onto a road segment.
pub struct SnappedWaypoint {
    pub seg_id: SegmentId,
    /// Whichever endpoint of `seg_id` is nearer (in arc-length) to the click.
    pub node: NodeId,
    /// Distance from `node` to the true click point, along the segment.
    pub offset_m: f64,
}

/// Snap `(lon, lat)` onto the road network per `hint` — see `SnapHint`. Used
/// for both the live-route preview and the final encode — both need "which
/// node do I route through, and how far is the true point from it" for
/// exactly the same reason `LineLocationInput` needs `start_offset_m`/
/// `end_offset_m`.
pub fn snap_point(graph: &Graph, lon: f64, lat: f64, hint: SnapHint) -> Option<SnappedWaypoint> {
    if let SnapHint::Node(node_id) = hint {
        if graph.nodes.contains_key(&node_id) {
            // Must be departable *from* this node in its permitted direction —
            // PAL reads this segment back out directly as its own line, with
            // no coverage-sweep/A* step afterward to reject an illegal
            // direction the way routing would. `topology_neighbors` ignores
            // `Direction` entirely (right for Rule-4's structural walk, wrong
            // here): picking an arbitrary touching segment could anchor PAL
            // on a one-way road in the prohibited direction, producing a
            // reference no decoder could ever route.
            let seg_id = graph.outgoing_segments(node_id).first().copied()?;
            return Some(SnappedWaypoint { seg_id, node: node_id, offset_m: 0.0 });
        }
        // Hinted node no longer loaded — fall through to nearest-segment search.
    }

    let seg_id = match hint {
        SnapHint::Segment(id) if graph.segments.contains_key(&id) => id,
        _ => {
            let nearby = graph.segments_near(lon, lat, WAYPOINT_SNAP_RADIUS_M);
            nearby.into_iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?.0
        }
    };
    let seg = graph.segments.get(&seg_id)?;
    let proj = project_onto_polyline(lon, lat, &seg.geometry)?;
    let total = seg.length_m;
    // Nearest-endpoint is only a free choice on a `Both`-direction segment.
    // A one-way segment can only be *anchored* at the end its direction
    // permits departing from — PAL reads this straight back out as its own
    // line with no coverage-sweep/A* step to reject the wrong choice
    // afterward (same root cause as the node-hint fix above, different
    // path: picking the geometrically nearer endpoint regardless of
    // direction could anchor PAL on the end that requires travelling the
    // prohibited way).
    match seg.direction {
        Direction::Forward  => Some(SnappedWaypoint { seg_id, node: seg.start_node, offset_m: proj.arc_offset_m }),
        Direction::Backward => Some(SnappedWaypoint { seg_id, node: seg.end_node, offset_m: total - proj.arc_offset_m }),
        Direction::Both => if proj.arc_offset_m <= total - proj.arc_offset_m {
            Some(SnappedWaypoint { seg_id, node: seg.start_node, offset_m: proj.arc_offset_m })
        } else {
            Some(SnappedWaypoint { seg_id, node: seg.end_node, offset_m: total - proj.arc_offset_m })
        },
    }
}

/// Outcome of chaining shortest-path search across an ordered waypoint list.
pub enum RouteOutcome {
    Found {
        path: Vec<SegmentId>,
        start_node: NodeId,
        start_offset_m: f64,
        end_offset_m: f64,
        length_m: f64,
        /// Segment-count boundaries within `path` marking where each
        /// waypoint-to-waypoint leg ends (see `LineLocationInput::via_split_points`).
        via_split_points: Vec<usize>,
        /// The snapped node coordinate for each waypoint, in input order.
        snapped_coords: Vec<(f64, f64)>,
    },
    NeedsTile(TileKey),
    Error(RouteFailure),
}

/// A `route_waypoints` failure, with structured leg context when the failure
/// is a specific waypoint-to-waypoint connection (as opposed to e.g. "no road
/// near this waypoint at all", which has no leg to report). When present,
/// `from_node`/`to_node`/`from_segment_id` can be fed directly into
/// `diagnose::diagnose_connection` without the caller having to look them up.
pub struct RouteFailure {
    pub message: String,
    pub from_node: Option<u32>,
    pub to_node: Option<u32>,
    pub from_segment_id: Option<u32>,
}

impl RouteFailure {
    fn plain(msg: impl Into<String>) -> Self {
        RouteFailure { message: msg.into(), from_node: None, to_node: None, from_segment_id: None }
    }
    fn leg(msg: impl Into<String>, from_node: NodeId, from_seg: SegmentId, to_node: NodeId) -> Self {
        RouteFailure {
            message: msg.into(),
            from_node: Some(from_node.0),
            to_node: Some(to_node.0),
            from_segment_id: if from_seg == NO_PRIOR_SEG { None } else { Some(from_seg.0) },
        }
    }
}

/// Snap every waypoint, then chain `shortest_path` leg-by-leg between
/// consecutive snaps. Shared by a live route-preview caller and
/// `line::encode_line`/`pal::encode_pal` (final encode) so both always see
/// the exact same routing decision — no stale-preview-vs-encode mismatch.
///
/// The first and last waypoints get special handling: when one snaps
/// mid-segment, its POFF/NOFF offset is only reconstructible by the decoder
/// if it's a forward distance from a node the recorded path genuinely
/// starts (or ends) at — and that's not necessarily the nearer endpoint of
/// the snapped segment. If the nearer endpoint happens to be the one the
/// route continues *away* from (its own segment never appears in the
/// recorded path at all), the offset is just a number with no path to trim
/// against, and the decoder reconstructs a bogus start/end point. Interior
/// waypoints don't have this problem — the Line format has no offset field
/// on interior LRPs, so nearest-endpoint snapping loses nothing extra.
///
/// `max_turn_deviation_deg` is the same cap `encode_line`'s `sweep_coverage`
/// step will enforce (see its own doc comment). Passing the real value here
/// — rather than a permissive `180.0` — means any path this preview finds is
/// already turn-angle-compliant, so `sweep_coverage`'s independent
/// re-derivation of that same path can't diverge over a turn this search
/// was allowed to take but that one wasn't: a route the preview shows as
/// connected is then guaranteed not to fail encoding for that reason.
pub fn route_waypoints(graph: &Graph, waypoints: &[Waypoint], max_turn_deviation_deg: f64, zoom: u8) -> RouteOutcome {
    if waypoints.len() < 2 {
        return RouteOutcome::Error(RouteFailure::plain("need at least 2 waypoints"));
    }

    let first_candidates = match boundary_candidates(graph, &waypoints[0]) {
        Some(c) => c,
        None => return RouteOutcome::Error(RouteFailure::plain(format!(
            "no road found within {WAYPOINT_SNAP_RADIUS_M}m of waypoint 0 — load more tiles or move it closer to a road"
        ))),
    };
    let last_idx = waypoints.len() - 1;
    let last_candidates = match boundary_candidates(graph, &waypoints[last_idx]) {
        Some(c) => c,
        None => return RouteOutcome::Error(RouteFailure::plain(format!(
            "no road found within {WAYPOINT_SNAP_RADIUS_M}m of waypoint {last_idx} — load more tiles or move it closer to a road"
        ))),
    };

    if waypoints.len() == 2 {
        // The one leg spans both boundaries at once — every (first, last)
        // candidate pair is a physically distinct route, so try them all
        // jointly rather than resolving each boundary independently.
        let mut best: Option<(usize, usize, PathResult, f64)> = None;
        for (fi, fc) in first_candidates.iter().enumerate() {
            for (li, lc) in last_candidates.iter().enumerate() {
                let (result, extra_len) = if fc.continuation == lc.continuation {
                    (PathResult { segments: vec![], length_m: 0.0 }, 0.0)
                } else {
                    match shortest_path(graph, fc.continuation, fc.bias_seg(), lc.continuation, 7, max_turn_deviation_deg, 0, zoom) {
                        PathOutcome::Found(r) => { let l = r.length_m; (r, l) }
                        PathOutcome::NeedsTile(tk) => return RouteOutcome::NeedsTile(tk),
                        PathOutcome::NoPath => continue,
                    }
                };
                let total = fc.offset_m + lc.offset_m + extra_len;
                if best.as_ref().map_or(true, |b| total < b.3) {
                    best = Some((fi, li, result, total));
                }
            }
        }
        let (fi, li, core, total) = match best {
            Some(b) => b,
            None => return RouteOutcome::Error(RouteFailure::plain("no route found between waypoint 0 and 1")),
        };
        let fc = &first_candidates[fi];
        let lc = &last_candidates[li];

        let mut full_path = Vec::with_capacity(core.segments.len() + 2);
        if fc.anchor != fc.continuation { full_path.push(fc.seg_id); }
        full_path.extend(core.segments);
        if lc.anchor != lc.continuation { full_path.push(lc.seg_id); }

        let snapped_coords = [fc.anchor, lc.anchor].iter()
            .filter_map(|n| graph.nodes.get(n).map(|n| (n.lon, n.lat)))
            .collect();

        return RouteOutcome::Found {
            path: full_path,
            start_node: fc.anchor,
            start_offset_m: fc.offset_m,
            end_offset_m: lc.offset_m,
            length_m: total,
            via_split_points: Vec::new(),
            snapped_coords,
        };
    }

    // Interior waypoints never carry an offset (Line format only supports
    // POFF/NOFF on the first/last LRP), so plain nearest-endpoint snapping
    // is fine — direction doesn't matter when there's no offset to
    // reconstruct.
    let mut mid_nodes = Vec::with_capacity(waypoints.len() - 2);
    for (i, w) in waypoints[1..last_idx].iter().enumerate() {
        match snap_point(graph, w.lon, w.lat, w.snap_hint()) {
            Some(s) => mid_nodes.push(s.node),
            None => return RouteOutcome::Error(RouteFailure::plain(format!(
                "no road found within {WAYPOINT_SNAP_RADIUS_M}m of waypoint {} — load more tiles or move it closer to a road", i + 1
            ))),
        }
    }

    let (fi, first_leg) = match resolve_boundary_leg(graph, &first_candidates, mid_nodes[0], max_turn_deviation_deg, zoom) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let fc = &first_candidates[fi];

    let mut full_path: Vec<SegmentId> = Vec::new();
    let mut current_seg = fc.bias_seg();
    let mut length_m = fc.offset_m;
    let mut via_split_points = Vec::new();

    if fc.anchor != fc.continuation { full_path.push(fc.seg_id); }
    length_m += first_leg.length_m;
    if let Some(&last) = first_leg.segments.last() { current_seg = last; }
    full_path.extend(first_leg.segments);
    // `mid_nodes[0]` is always an interior waypoint here (this branch only
    // runs for len >= 3), so this leg always ends at a real via-point.
    via_split_points.push(full_path.len());

    // Interior-to-interior legs: unaffected by the boundary-offset problem
    // (no offset to reconstruct), so a plain chained search is fine. Every
    // one of these ends at another interior waypoint, so each gets a split
    // point too.
    for i in 0..mid_nodes.len() - 1 {
        match shortest_path(graph, mid_nodes[i], current_seg, mid_nodes[i + 1], 7, max_turn_deviation_deg, 0, zoom) {
            PathOutcome::Found(r) => {
                length_m += r.length_m;
                if let Some(&last) = r.segments.last() { current_seg = last; }
                full_path.extend(r.segments);
                via_split_points.push(full_path.len());
            }
            PathOutcome::NoPath => return RouteOutcome::Error(RouteFailure::leg(
                format!("no route found between waypoint {} and {}", i + 1, i + 2),
                mid_nodes[i], current_seg, mid_nodes[i + 1],
            )),
            PathOutcome::NeedsTile(tk) => return RouteOutcome::NeedsTile(tk),
        }
    }

    let last_mid = *mid_nodes.last().unwrap();
    let (li, last_leg) = match resolve_boundary_leg_from(graph, last_mid, current_seg, &last_candidates, max_turn_deviation_deg, zoom) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let lc = &last_candidates[li];
    length_m += last_leg.length_m + lc.offset_m;
    full_path.extend(last_leg.segments);
    if lc.anchor != lc.continuation { full_path.push(lc.seg_id); }

    let mut snapped_coords: Vec<(f64, f64)> = Vec::with_capacity(waypoints.len());
    snapped_coords.extend(graph.nodes.get(&fc.anchor).map(|n| (n.lon, n.lat)));
    snapped_coords.extend(mid_nodes.iter().filter_map(|n| graph.nodes.get(n).map(|n| (n.lon, n.lat))));
    snapped_coords.extend(graph.nodes.get(&lc.anchor).map(|n| (n.lon, n.lat)));

    RouteOutcome::Found {
        path: full_path,
        start_node: fc.anchor,
        start_offset_m: fc.offset_m,
        end_offset_m: lc.offset_m,
        length_m,
        via_split_points,
        snapped_coords,
    }
}

/// One nearby place a raw click could snap onto, for disambiguating a click
/// that lands near more than one plausible road or intersection. `kind` is
/// `"node"` (a real intersection/junction — snapping here means an exact,
/// zero-offset anchor) or `"segment"` (a point along a road's interior, some
/// distance from either endpoint).
#[derive(Debug)]
pub struct NearbyAnchor {
    pub kind: &'static str,
    pub node_id: Option<NodeId>,
    pub segment_id: Option<SegmentId>,
    pub distance_m: f64,
    pub point: (f64, f64),
    pub frc: Option<u8>,
    pub fow: Option<u8>,
    pub stable_id: Option<String>,
}

/// Nearby road/intersection candidates a click at `(lon, lat)` could snap
/// onto — nodes first (exact, zero-offset choices), then along-segment
/// points, nearest first, with any segment candidate essentially coincident
/// with an already-listed node (e.g. a road's very endpoint) or another
/// segment candidate (e.g. the reverse-direction copy of the same road)
/// filtered out as a duplicate rather than offered as a separate choice.
pub fn nearby_anchors(graph: &Graph, lon: f64, lat: f64) -> Vec<NearbyAnchor> {
    let nearby_segs = graph.segments_near(lon, lat, WAYPOINT_SNAP_RADIUS_M);

    let mut seen_nodes: HashSet<NodeId> = HashSet::new();
    let mut candidates: Vec<NearbyAnchor> = Vec::new();
    for (seg_id, _) in &nearby_segs {
        let Some(seg) = graph.segments.get(seg_id) else { continue };
        for node_id in [seg.start_node, seg.end_node] {
            if !seen_nodes.insert(node_id) { continue; }
            let Some(dist) = graph.node_dist_m(node_id, lon, lat) else { continue };
            if dist > WAYPOINT_SNAP_RADIUS_M { continue; }
            let Some(n) = graph.nodes.get(&node_id) else { continue };
            candidates.push(NearbyAnchor {
                kind: "node",
                node_id: Some(node_id),
                segment_id: None,
                distance_m: dist,
                point: (n.lon, n.lat),
                frc: None,
                fow: None,
                stable_id: None,
            });
        }
    }

    let close_enough = |a: (f64, f64), b: (f64, f64)| {
        let dx = (a.0 - b.0) * a.1.to_radians().cos();
        let dy = a.1 - b.1;
        (dx * dx + dy * dy).sqrt() * 111_000.0 < 5.0
    };
    let mut seg_candidates: Vec<NearbyAnchor> = nearby_segs.into_iter()
        .filter_map(|(seg_id, _dist)| {
            let seg = graph.segments.get(&seg_id)?;
            let proj = project_onto_polyline(lon, lat, &seg.geometry)?;
            Some(NearbyAnchor {
                kind: "segment",
                node_id: None,
                segment_id: Some(seg_id),
                distance_m: proj.distance_m,
                point: proj.point,
                frc: Some(seg.frc),
                fow: Some(seg.fow),
                stable_id: Some(seg.stable_id.clone()),
            })
        })
        .collect();
    seg_candidates.sort_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap());
    for c in seg_candidates {
        let is_dup = candidates.iter().any(|d| close_enough(d.point, c.point));
        if !is_dup {
            candidates.push(c);
        }
    }

    candidates.sort_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap());
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::NetworkNode;

    // A straight line of 4 nodes, ~111m apart (0.001° longitude at the
    // equator), all Both-direction unless noted: 0 --seg1--> 1 --seg2--> 2 --seg3--> 3
    fn line_graph() -> Graph {
        let mut g = Graph::new();
        for i in 0..4u32 {
            g.add_node(NetworkNode { id: NodeId(i), lon: i as f64 * 0.001, lat: 0.0, stable_id: String::new(), is_boundary: false });
        }
        for i in 0..3u32 {
            g.add_segment(openlr_graph::NetworkSegment {
                id: SegmentId(i + 1), start_node: NodeId(i), end_node: NodeId(i + 1),
                geometry: vec![(i as f64 * 0.001, 0.0), ((i + 1) as f64 * 0.001, 0.0)],
                length_m: 111.2, frc: 3, fow: 3, direction: Direction::Both, stable_id: String::new(),
            });
        }
        g
    }

    fn wp(lon: f64, lat: f64) -> Waypoint {
        Waypoint { lon, lat, segment_id: None, node_id: None }
    }

    #[test]
    fn snap_point_node_hint_returns_exact_zero_offset() {
        let g = line_graph();
        let s = snap_point(&g, 0.001, 0.0, SnapHint::Node(NodeId(1))).unwrap();
        assert_eq!(s.node, NodeId(1));
        assert_eq!(s.offset_m, 0.0);
    }

    #[test]
    fn snap_point_nearest_picks_the_closer_endpoint_on_a_both_direction_segment() {
        let g = line_graph();
        // Just past node 1, along seg2 — closer to node 1 than node 2.
        let s = snap_point(&g, 0.0011, 0.0, SnapHint::Nearest).unwrap();
        assert_eq!(s.seg_id, SegmentId(2));
        assert_eq!(s.node, NodeId(1));
        assert!(s.offset_m > 0.0 && s.offset_m < 55.0, "offset_m={}", s.offset_m);
    }

    #[test]
    fn snap_point_forward_segment_always_anchors_at_start_node() {
        let mut g = line_graph();
        g.add_segment(openlr_graph::NetworkSegment {
            id: SegmentId(1), start_node: NodeId(0), end_node: NodeId(1),
            geometry: vec![(0.0, 0.0), (0.001, 0.0)],
            length_m: 111.2, frc: 3, fow: 3, direction: Direction::Forward, stable_id: String::new(),
        });
        // Click near the *far* end (node 1) — a Backward/Both snap would prefer
        // node 1, but Forward must anchor at the start regardless (Invariant 10:
        // no downstream A*/routing step here to catch a wrong-direction pick).
        let s = snap_point(&g, 0.0009, 0.0, SnapHint::Nearest).unwrap();
        assert_eq!(s.node, NodeId(0));
        assert!(s.offset_m > 80.0, "offset_m={} should be near the full segment length", s.offset_m);
    }

    #[test]
    fn boundary_candidates_node_hint_is_a_single_zero_offset_candidate() {
        let g = line_graph();
        let cands = boundary_candidates(&g, &Waypoint { lon: 0.001, lat: 0.0, segment_id: None, node_id: Some(1) }).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].anchor, NodeId(1));
        assert_eq!(cands[0].continuation, NodeId(1));
        assert_eq!(cands[0].offset_m, 0.0);
    }

    #[test]
    fn boundary_candidates_mid_segment_offers_both_endpoints() {
        let g = line_graph();
        let cands = boundary_candidates(&g, &wp(0.0015, 0.0)).unwrap();
        assert_eq!(cands.len(), 2);
        let anchors: Vec<NodeId> = cands.iter().map(|c| c.anchor).collect();
        assert!(anchors.contains(&NodeId(1)) && anchors.contains(&NodeId(2)));
        // Offsets from either end should sum to the segment length.
        assert!((cands[0].offset_m + cands[1].offset_m - 111.2).abs() < 1.0);
    }

    #[test]
    fn nearby_anchors_does_not_duplicate_a_node_as_a_segment_candidate() {
        let g = line_graph();
        let anchors = nearby_anchors(&g, 0.001, 0.0); // exactly at node 1
        let nodes: Vec<_> = anchors.iter().filter(|a| a.kind == "node").collect();
        let segs: Vec<_> = anchors.iter().filter(|a| a.kind == "segment").collect();
        assert_eq!(nodes.len(), 1, "exactly one node candidate for node 1 itself");
        assert_eq!(nodes[0].node_id, Some(NodeId(1)));
        assert!(segs.is_empty(), "seg1/seg2 touching node 1 exactly here must not duplicate it: {segs:?}");
    }

    #[test]
    fn route_waypoints_two_points_covers_the_whole_line() {
        let g = line_graph();
        let waypoints = vec![
            Waypoint { lon: 0.0, lat: 0.0, segment_id: None, node_id: Some(0) },
            Waypoint { lon: 0.003, lat: 0.0, segment_id: None, node_id: Some(3) },
        ];
        match route_waypoints(&g, &waypoints, 180.0, 12) {
            RouteOutcome::Found { path, start_node, start_offset_m, end_offset_m, length_m, .. } => {
                assert_eq!(path, vec![SegmentId(1), SegmentId(2), SegmentId(3)]);
                assert_eq!(start_node, NodeId(0));
                assert_eq!(start_offset_m, 0.0);
                assert_eq!(end_offset_m, 0.0);
                assert!((length_m - 333.6).abs() < 1.0);
            }
            _ => panic!("expected Found, got a different outcome"),
        }
    }

    #[test]
    fn route_waypoints_three_points_records_a_via_split() {
        let g = line_graph();
        let waypoints = vec![
            Waypoint { lon: 0.0, lat: 0.0, segment_id: None, node_id: Some(0) },
            Waypoint { lon: 0.002, lat: 0.0, segment_id: None, node_id: Some(2) },
            Waypoint { lon: 0.003, lat: 0.0, segment_id: None, node_id: Some(3) },
        ];
        match route_waypoints(&g, &waypoints, 180.0, 12) {
            RouteOutcome::Found { path, via_split_points, .. } => {
                assert_eq!(path, vec![SegmentId(1), SegmentId(2), SegmentId(3)]);
                // The first leg (waypoint 0 -> 1) covers segments 1 and 2, so the
                // split point lands after 2 segments.
                assert_eq!(via_split_points, vec![2]);
            }
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn route_waypoints_rejects_a_single_waypoint() {
        let g = line_graph();
        let waypoints = vec![wp(0.0, 0.0)];
        match route_waypoints(&g, &waypoints, 180.0, 12) {
            RouteOutcome::Error(f) => assert!(f.message.contains("at least 2")),
            _ => panic!("expected an Error outcome"),
        }
    }

    #[test]
    fn route_waypoints_reports_no_road_near_an_isolated_waypoint() {
        let g = line_graph();
        // Far from any segment in line_graph (>> WAYPOINT_SNAP_RADIUS_M away).
        let waypoints = vec![wp(0.0, 0.0), wp(10.0, 10.0)];
        match route_waypoints(&g, &waypoints, 180.0, 12) {
            RouteOutcome::Error(f) => assert!(f.message.contains("no road found")),
            _ => panic!("expected an Error outcome"),
        }
    }
}
