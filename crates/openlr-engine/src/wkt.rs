use openlr_graph::{Graph, NodeId, SegmentId, haversine_m, interpolate_at, polyline_length_m};
use crate::trace::TraversalDir;

/// Assemble a WKT `LINESTRING` from a decoded path, applying pos/neg offsets.
///
/// Offsets are measured relative to the LRP projection points, not segment endpoints:
/// - `first_lrp_arc_m`: arc offset of the first LRP on the first segment (traversal dir).
///   Actual start = `first_lrp_arc_m + pos_offset_m` along the path.
/// - `last_lrp_arc_m`: arc offset of the last LRP on the last segment (traversal dir).
///   Actual end = `last_lrp_arc_m - neg_offset_m` along the path.
///
/// Both offsets can overflow their segment — the excess carries into adjacent segments.
///
/// Returns `None` if any segment is missing from the graph or the trimmed
/// result collapses to fewer than 2 points.
pub fn path_to_wkt(
    path: &[SegmentId],
    pos_offset_m: f64,
    neg_offset_m: f64,
    first_lrp_arc_m: f64,
    last_lrp_arc_m: f64,
    first_seg_traversal: TraversalDir,
    _last_seg_traversal: TraversalDir,
    graph: &Graph,
) -> Option<String> {
    if path.is_empty() {
        return None;
    }

    let n = path.len();

    // Resolve all segments up front; bail if any is missing.
    let segs: Vec<_> = path.iter().map(|id| graph.segments.get(id)).collect::<Option<Vec<_>>>()?;

    // Traversal direction per segment: Forward = stored geometry order.
    // seg[0] uses the explicit traversal direction from candidate selection —
    // the heuristic (comparing end_node with seg[1]'s endpoints) can fail when
    // A* U-turns back through the departure segment, making seg[1] appear to
    // share seg[0].end_node even though seg[0] is traversed Backward.
    // Segments [1..n-1] are inferred from node-connectivity; A* guarantees they
    // connect correctly, so the chain inference is sound for them.
    let mut forward = vec![true; n];
    forward[0] = matches!(first_seg_traversal, TraversalDir::Forward);
    if n >= 2 {
        for i in 1..n {
            let prev_exit: NodeId = if forward[i - 1] { segs[i - 1].end_node } else { segs[i - 1].start_node };
            forward[i] = segs[i].start_node == prev_exit;
        }
    }

    // Precompute haversine lengths (same regardless of traversal direction).
    let actual_lens: Vec<f64> = segs.iter()
        .map(|seg| polyline_length_m(&seg.geometry))
        .collect();

    let (pos_seg, pos_start_m, neg_seg, neg_end_m) =
        find_coverage_bounds(&actual_lens, pos_offset_m, neg_offset_m, first_lrp_arc_m, last_lrp_arc_m)?;

    let mut pts: Vec<(f64, f64)> = Vec::new();

    for (i, (seg, &fwd)) in segs.iter().zip(forward.iter()).enumerate() {
        // Skip segments entirely outside the trim window.
        if i < pos_seg || i > neg_seg {
            continue;
        }

        let geom: Vec<(f64, f64)> = if fwd {
            seg.geometry.clone()
        } else {
            seg.geometry.iter().cloned().rev().collect()
        };

        let actual_len = actual_lens[i];
        let start_m = if i == pos_seg { pos_start_m } else { 0.0 };
        let end_m   = if i == neg_seg { neg_end_m   } else { actual_len };

        if end_m <= start_m {
            continue;
        }

        let seg_pts = segment_vertices(&geom, actual_len, start_m, end_m);

        if pts.is_empty() {
            pts.extend_from_slice(&seg_pts);
        } else if let Some(first) = seg_pts.first() {
            // Segments share a junction vertex — skip the duplicate.
            let last = *pts.last().unwrap();
            let dup = (last.0 - first.0).abs() < 1e-8 && (last.1 - first.1).abs() < 1e-8;
            pts.extend_from_slice(if dup { &seg_pts[1..] } else { &seg_pts });
        }
    }

    if pts.len() < 2 {
        return None;
    }

    let coords = pts.iter()
        .map(|(lon, lat)| format!("{lon:.7} {lat:.7}"))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("LINESTRING ({coords})"))
}

/// Find which segment index the offset-adjusted location starts/ends at, and
/// the arc-length position within that segment where coverage begins/ends.
/// Shared by `path_to_wkt` and `coverage_range` so the two stay consistent.
///
/// Returns `None` if the trim window is empty or inverted (location collapsed).
fn find_coverage_bounds(
    actual_lens: &[f64],
    pos_offset_m: f64,
    neg_offset_m: f64,
    first_lrp_arc_m: f64,
    last_lrp_arc_m: f64,
) -> Option<(usize, f64, usize, f64)> {
    let n = actual_lens.len();

    // ── Positive-offset cut: walk forward from (first_lrp_arc_m + pos_offset_m) ──
    // Finds the segment index and within-segment start where the location begins.
    let (pos_seg, pos_start_m) = {
        let mut rem = (first_lrp_arc_m + pos_offset_m).max(0.0);
        let mut result = (0usize, 0.0f64);
        for i in 0..n {
            if rem <= actual_lens[i] {
                result = (i, rem);
                break;
            }
            rem -= actual_lens[i];
        }
        result
    };

    // ── Negative-offset cut: walk backward from last LRP position ──
    // Finds the segment index and within-segment end where the location ends.
    let (neg_seg, neg_end_m) = {
        let lrp_arc = last_lrp_arc_m.min(actual_lens[n - 1]);
        let mut rem = neg_offset_m;
        if rem <= lrp_arc {
            // Trim lands within the last segment.
            (n - 1, lrp_arc - rem)
        } else {
            rem -= lrp_arc;
            let mut result = (0usize, 0.0f64); // fallback: entire path consumed
            'neg_cut: for i in (0..n - 1).rev() {
                let avail = actual_lens[i];
                if rem <= avail {
                    result = (i, avail - rem);
                    break 'neg_cut;
                }
                rem -= avail;
            }
            result
        }
    };

    // If the trim window is empty or inverted, the location has collapsed.
    if pos_seg > neg_seg { return None; }
    if pos_seg == neg_seg && pos_start_m >= neg_end_m { return None; }

    Some((pos_seg, pos_start_m, neg_seg, neg_end_m))
}

/// Segment-index range that's at least partially covered by the conservative
/// (LB-trimmed) location, plus the residual pos/neg offset -- in meters, as a
/// [lb, ub] interval -- measured from that first/last covered segment's own
/// start/end (rather than from the original LRP position). For callers that
/// want a segment-pruned view of the location: e.g. exporting GeoJSON
/// features only for segments the location actually touches, while still
/// reporting a meaningful offset relative to *those* segments' boundaries.
///
/// Segment *selection* (which index is first/last covered) is fixed using
/// the LB bound, matching `path_to_wkt`'s own "conservative, maximal
/// coverage" semantics -- the UB residual is derived by shifting the LB
/// residual by the original offset interval's width, since dropping the same
/// whole segments shifts both bounds of an interval by the same constant (no
/// midpoint/estimate anywhere, both bounds are exact given that shared
/// segment selection).
///
/// Returns `None` if any segment is missing from the graph, or the (LB-based)
/// trim window is empty/inverted (location collapsed).
pub struct CoverageRange {
    pub first_segment_idx: usize,
    pub last_segment_idx: usize,
    pub pos_residual_lb: f64,
    pub pos_residual_ub: f64,
    pub neg_residual_lb: f64,
    pub neg_residual_ub: f64,
}

pub fn coverage_range(
    path: &[SegmentId],
    pos_offset_lb: f64,
    pos_offset_ub: f64,
    neg_offset_lb: f64,
    neg_offset_ub: f64,
    first_lrp_arc_m: f64,
    last_lrp_arc_m: f64,
    graph: &Graph,
) -> Option<CoverageRange> {
    if path.is_empty() {
        return None;
    }
    let segs: Vec<_> = path.iter().map(|id| graph.segments.get(id)).collect::<Option<Vec<_>>>()?;
    let actual_lens: Vec<f64> = segs.iter().map(|seg| polyline_length_m(&seg.geometry)).collect();

    let (pos_seg, pos_start_m, neg_seg, neg_end_m) =
        find_coverage_bounds(&actual_lens, pos_offset_lb, neg_offset_lb, first_lrp_arc_m, last_lrp_arc_m)?;

    let pos_width = pos_offset_ub - pos_offset_lb;
    let neg_width = neg_offset_ub - neg_offset_lb;
    let neg_residual_lb = actual_lens[neg_seg] - neg_end_m;

    Some(CoverageRange {
        first_segment_idx: pos_seg,
        last_segment_idx: neg_seg,
        pos_residual_lb: pos_start_m,
        pos_residual_ub: pos_start_m + pos_width,
        neg_residual_lb,
        neg_residual_ub: neg_residual_lb + neg_width,
    })
}

/// Extract vertices from a polyline between [start_m, end_m] arc-length offsets.
///
/// `actual_len` is the pre-computed haversine length of the polyline (avoids
/// recomputing inside the function when the caller already has it).
fn segment_vertices(
    geom: &[(f64, f64)],
    actual_len: f64,
    start_m: f64,
    end_m: f64,
) -> Vec<(f64, f64)> {
    // Snap to exact endpoints when we're not trimming — avoids FP drift from
    // interpolate_at when stored segment length differs from haversine length.
    let start_pt = if start_m <= 0.0 { geom[0] } else { interpolate_at(geom, start_m) };
    let end_pt   = if end_m >= actual_len { *geom.last().unwrap() } else { interpolate_at(geom, end_m) };

    let mut out = vec![start_pt];
    let mut acc = 0.0;
    for w in geom.windows(2) {
        acc += haversine_m(w[0].0, w[0].1, w[1].0, w[1].1);
        // Include vertex w[1] only when its arc-length is strictly inside the window.
        if acc > start_m && acc < end_m {
            out.push(w[1]);
        }
    }

    // Append the end point unless it's already the last collected point.
    let last = *out.last().unwrap();
    if (last.0 - end_pt.0).abs() > 1e-9 || (last.1 - end_pt.1).abs() > 1e-9 {
        out.push(end_pt);
    }

    out
}

/// Extract a WKT linestring for a band of the path between two arc-length positions,
/// both measured from the traversal entry of the first segment.
///
/// This is used to render offset-uncertainty bands: the caller passes the LB and UB
/// of the offset interval (converted to absolute arc positions from path start), and
/// gets back the geometry of that uncertainty zone.
///
/// Implemented by delegating to `path_to_wkt` with synthetic trim parameters:
/// - start trim = `from_m` (treated as a pos-offset from a zero-arc-length first LRP)
/// - end trim   = `total_path_len - to_m` (treated as a neg-offset from a full-length last LRP)
pub fn path_band_wkt(
    path: &[SegmentId],
    from_m: f64,
    to_m: f64,
    first_seg_traversal: TraversalDir,
    graph: &Graph,
) -> Option<String> {
    if from_m >= to_m { return None; }

    let segs: Vec<_> = path.iter()
        .map(|id| graph.segments.get(id))
        .collect::<Option<Vec<_>>>()?;

    let actual_lens: Vec<f64> = segs.iter()
        .map(|s| polyline_length_m(&s.geometry))
        .collect();
    let total_len: f64 = actual_lens.iter().sum();
    let last_seg_len = *actual_lens.last()?;

    let neg_trim = (total_len - to_m).max(0.0);

    path_to_wkt(
        path,
        from_m.max(0.0),
        neg_trim,
        0.0,           // first_lrp_arc_m = 0 (LRP at entry of first seg)
        last_seg_len,  // last_lrp_arc_m = full length (LRP at exit of last seg)
        first_seg_traversal,
        TraversalDir::Forward, // _last_seg_traversal is unused by path_to_wkt
        graph,
    )
}

/// Walk `dist_m` meters from the start of the path (applying `first_seg_traversal` to the
/// first segment, Forward to all subsequent segments) and return the interpolated WGS84
/// coordinate (lon, lat) at that position.
///
/// Used for PointAlongLine decoding: `dist_m = first_lrp_arc_m + pos_offset_m`.
pub fn point_at_path_distance(
    path: &[SegmentId],
    dist_m: f64,
    first_seg_traversal: TraversalDir,
    graph: &Graph,
) -> Option<(f64, f64)> {
    // Collect all vertices of the path in traversal order into one flat list.
    // Consecutive segments share an endpoint — deduplicate at segment boundaries.
    let mut all_verts: Vec<(f64, f64)> = Vec::new();
    for (i, seg_id) in path.iter().enumerate() {
        let seg = graph.segments.get(seg_id)?;
        let trav = if i == 0 { first_seg_traversal } else { TraversalDir::Forward };
        let verts: &[(f64, f64)] = &seg.geometry;
        let ordered: Vec<(f64, f64)> = match trav {
            TraversalDir::Forward  => verts.to_vec(),
            TraversalDir::Backward => verts.iter().rev().copied().collect(),
        };
        if i == 0 {
            all_verts.extend_from_slice(&ordered);
        } else {
            // Skip the first vertex (it's the same as the last of the previous segment).
            all_verts.extend_from_slice(&ordered[1..]);
        }
    }
    if all_verts.is_empty() { return None; }
    Some(interpolate_at(&all_verts, dist_m.max(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, Graph, NetworkNode, NetworkSegment, NodeId, SegmentId};
        use crate::trace::TraversalDir;

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: String::new(), is_boundary: false }
    }
    fn seg_g(id: u32, s: u32, e: u32, geom: Vec<(f64, f64)>) -> NetworkSegment {
        let len = polyline_length_m(&geom);
        NetworkSegment {
            id: SegmentId(id), start_node: NodeId(s), end_node: NodeId(e),
            geometry: geom, length_m: len, frc: 3, fow: 3, direction: Direction::Both,
            stable_id: String::new(),
        }
    }

    #[test]
    fn no_offsets_two_segments() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg_g(1, 0, 1, vec![(0.0, 0.0), (0.001, 0.0)]));
        g.add_segment(seg_g(2, 1, 2, vec![(0.001, 0.0), (0.002, 0.0)]));

        let seg1_len = polyline_length_m(&[(0.0_f64, 0.0_f64), (0.001, 0.0)]);
        let seg2_len = polyline_length_m(&[(0.001_f64, 0.0_f64), (0.002, 0.0)]);
        // LRPs at nodes: first_lrp_arc = 0, last_lrp_arc = seg2_len.
        let wkt = path_to_wkt(&[SegmentId(1), SegmentId(2)], 0.0, 0.0, 0.0, seg2_len, TraversalDir::Forward, TraversalDir::Forward, &g).unwrap();
        // start, junction (deduped), end → 3 points
        assert!(wkt.starts_with("LINESTRING ("), "{wkt}");
        let n_pts = wkt.split(',').count();
        assert_eq!(n_pts, 3, "expected 3 points (start, junction, end): {wkt}");
        let _ = seg1_len; // suppress unused-variable warning
    }

    #[test]
    fn pos_offset_trims_start() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.01, 0.0)); // ~1.1 km
        g.add_segment(seg_g(1, 0, 1, vec![(0.0, 0.0), (0.01, 0.0)]));

        let len = polyline_length_m(&[(0.0_f64, 0.0_f64), (0.01, 0.0)]);
        // Trim the first 20 % from the start.  LRP at node 0 (arc = 0), last LRP at node 1 (arc = len).
        let offset = len * 0.2;
        let wkt = path_to_wkt(&[SegmentId(1)], offset, 0.0, 0.0, len, TraversalDir::Forward, TraversalDir::Forward, &g).unwrap();
        // The start point should be offset from (0,0).
        assert!(!wkt.contains("0.0000000 0.0000000"), "start should be trimmed: {wkt}");
    }

    #[test]
    fn empty_path_returns_none() {
        let g = Graph::new();
        assert!(path_to_wkt(&[], 0.0, 0.0, 0.0, 0.0, TraversalDir::Forward, TraversalDir::Forward, &g).is_none());
    }

    fn three_segment_graph() -> (Graph, f64, f64, f64) {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_node(node(3, 0.003, 0.0));
        g.add_segment(seg_g(1, 0, 1, vec![(0.0, 0.0), (0.001, 0.0)]));
        g.add_segment(seg_g(2, 1, 2, vec![(0.001, 0.0), (0.002, 0.0)]));
        g.add_segment(seg_g(3, 2, 3, vec![(0.002, 0.0), (0.003, 0.0)]));
        let len1 = polyline_length_m(&[(0.0_f64, 0.0_f64), (0.001, 0.0)]);
        let len2 = polyline_length_m(&[(0.001_f64, 0.0_f64), (0.002, 0.0)]);
        let len3 = polyline_length_m(&[(0.002_f64, 0.0_f64), (0.003, 0.0)]);
        (g, len1, len2, len3)
    }

    #[test]
    fn coverage_range_no_offsets_covers_whole_path() {
        let (g, _len1, _len2, len3) = three_segment_graph();
        let path = [SegmentId(1), SegmentId(2), SegmentId(3)];
        let cr = coverage_range(&path, 0.0, 0.0, 0.0, 0.0, 0.0, len3, &g).unwrap();
        assert_eq!(cr.first_segment_idx, 0);
        assert_eq!(cr.last_segment_idx, 2);
        assert_eq!(cr.pos_residual_lb, 0.0);
        assert_eq!(cr.pos_residual_ub, 0.0);
        assert_eq!(cr.neg_residual_lb, 0.0);
        assert_eq!(cr.neg_residual_ub, 0.0);
    }

    #[test]
    fn coverage_range_pos_offset_skips_fully_consumed_leading_segment() {
        let (g, len1, _len2, len3) = three_segment_graph();
        let path = [SegmentId(1), SegmentId(2), SegmentId(3)];
        // Positive offset fully consumes segment 1 (index 0) and reaches 20 m
        // into segment 2 (index 1) at the LB, 30 m in at the UB.
        let pos_lb = len1 + 20.0;
        let pos_ub = len1 + 30.0;
        let cr = coverage_range(&path, pos_lb, pos_ub, 0.0, 0.0, 0.0, len3, &g).unwrap();
        assert_eq!(cr.first_segment_idx, 1, "segment 1 (fully consumed) must be excluded");
        assert_eq!(cr.last_segment_idx, 2);
        assert!((cr.pos_residual_lb - 20.0).abs() < 1e-6, "{}", cr.pos_residual_lb);
        assert!((cr.pos_residual_ub - 30.0).abs() < 1e-6, "{}", cr.pos_residual_ub);
        // UB residual must be exactly LB residual + interval width -- no midpoint anywhere.
        assert!((cr.pos_residual_ub - cr.pos_residual_lb - (pos_ub - pos_lb)).abs() < 1e-9);
        assert_eq!(cr.neg_residual_lb, 0.0);
        assert_eq!(cr.neg_residual_ub, 0.0);
    }

    #[test]
    fn coverage_range_neg_offset_skips_fully_consumed_trailing_segment() {
        let (g, _len1, len2, len3) = three_segment_graph();
        let path = [SegmentId(1), SegmentId(2), SegmentId(3)];
        // Negative offset fully consumes segment 3 (index 2) and reaches 15 m
        // back into segment 2 (index 1) at the LB, 25 m back at the UB.
        let neg_lb = len3 + 15.0;
        let neg_ub = len3 + 25.0;
        let cr = coverage_range(&path, 0.0, 0.0, neg_lb, neg_ub, 0.0, len3, &g).unwrap();
        assert_eq!(cr.first_segment_idx, 0);
        assert_eq!(cr.last_segment_idx, 1, "segment 3 (fully consumed) must be excluded");
        assert_eq!(cr.pos_residual_lb, 0.0);
        assert_eq!(cr.pos_residual_ub, 0.0);
        assert!((cr.neg_residual_lb - 15.0).abs() < 1e-6, "{}", cr.neg_residual_lb);
        assert!((cr.neg_residual_ub - 25.0).abs() < 1e-6, "{}", cr.neg_residual_ub);
        assert!((cr.neg_residual_ub - cr.neg_residual_lb - (neg_ub - neg_lb)).abs() < 1e-9);
        let _ = len2;
    }

    #[test]
    fn coverage_range_empty_path_returns_none() {
        let g = Graph::new();
        assert!(coverage_range(&[], 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, &g).is_none());
    }
}
