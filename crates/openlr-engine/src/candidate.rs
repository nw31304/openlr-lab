use openlr_codec::lrp::Lrp;
use openlr_graph::{bearing_at_offset, project_onto_polyline, Direction, Graph, SegmentId};

use crate::params::DecodeParams;
use crate::trace::{
    CandidateScore, DecodeEvent, GateVerdict, ProjectionResult, RejectedCandidate,
    ScoredCandidate, TraversalDir, DecodeTrace,
};

/// Select and rank candidate segments for one LRP.
///
/// Returns the accepted candidates sorted ascending by total score (best first).
/// Rejects are counted but not returned unless `trace_level == Full`
/// (they appear in `CandidateEvaluated` events).
pub fn select_candidates(
    lrp_idx: usize,
    lrp: &Lrp,
    is_last_lrp: bool,
    graph: &Graph,
    params: &DecodeParams,
    trace: &mut DecodeTrace,
) -> Vec<ScoredCandidate> {
    let (lon, lat) = lrp.coord;

    let nearby = graph.segments_near(lon, lat, params.candidate_search_radius_m);

    trace.push_summary(DecodeEvent::CandidateSearchStarted {
        lrp_idx,
        coord: lrp.coord,
        radius_m: params.candidate_search_radius_m,
    });

    let mut accepted: Vec<ScoredCandidate> = Vec::new();
    let mut rejected: Vec<RejectedCandidate> = Vec::new();

    for &(seg_id, _coarse_dist) in &nearby {
        let seg = match graph.segments.get(&seg_id) {
            Some(s) => s,
            None => continue,
        };

        // A bidirectional segment becomes two one-way candidates (Forward and Backward).
        // A one-way segment yields only the legal traversal direction.
        // Direction::Backward means legal travel is end→start (OSM oneway=-1).
        let dirs: &[TraversalDir] = match seg.direction {
            Direction::Both     => &[TraversalDir::Forward, TraversalDir::Backward],
            Direction::Forward  => &[TraversalDir::Forward],
            Direction::Backward => &[TraversalDir::Backward],
        };

        for &dir in dirs {
            match evaluate_candidate(lrp, seg_id, dir, is_last_lrp, seg, params) {
                EvalResult::Accepted(scored) => {
                    trace.push_full(DecodeEvent::CandidateEvaluated {
                        lrp_idx,
                        segment_id: seg_id,
                        traversal: dir,
                        projection: scored.projection.clone(),
                        verdict: GateVerdict::Pass,
                        score: Some(scored.score.clone()),
                    });
                    accepted.push(scored);
                }
                EvalResult::Rejected { verdict, distance_m, point, bearing_deg } => {
                    trace.push_full(DecodeEvent::CandidateEvaluated {
                        lrp_idx,
                        segment_id: seg_id,
                        traversal: dir,
                        projection: ProjectionResult {
                            arc_offset_m: 0.0,
                            point: point.unwrap_or((0.0, 0.0)),
                            distance_m: distance_m.unwrap_or(0.0),
                            bearing_deg: bearing_deg.unwrap_or(0.0),
                            is_at_entry: false,
                            is_at_exit: false,
                        },
                        verdict: verdict.clone(),
                        score: None,
                    });
                    rejected.push(RejectedCandidate {
                        segment_id: seg_id,
                        traversal: dir,
                        distance_m,
                        point,
                        bearing_deg,
                        verdict,
                    });
                }
            }
        }
    }

    // Sort ascending by total score (lower = better), then cap to max_candidates_per_lrp.
    accepted.sort_by(|a, b| {
        a.score.total.partial_cmp(&b.score.total).unwrap_or(std::cmp::Ordering::Equal)
    });
    if params.max_candidates_per_lrp > 0 {
        accepted.truncate(params.max_candidates_per_lrp);
    }

    trace.push_summary(DecodeEvent::CandidatesRanked {
        lrp_idx,
        accepted: accepted.clone(),
        rejected,
        segments_fetched: nearby.len(),
    });

    accepted
}

// ── Internal ──────────────────────────────────────────────────────────────────

/// Result of evaluating a single candidate segment/direction pair.
enum EvalResult {
    Accepted(ScoredCandidate),
    Rejected {
        verdict: GateVerdict,
        /// Distance from LRP to projected point. `None` only for `FailDirection`.
        distance_m: Option<f64>,
        /// Snap point (lon, lat). `None` only for `FailDirection`.
        point: Option<(f64, f64)>,
        /// Bearing at the projection point. `None` for `FailDirection` and `FailRadius`.
        bearing_deg: Option<f64>,
    },
}

fn evaluate_candidate(
    lrp: &Lrp,
    seg_id: SegmentId,
    dir: TraversalDir,
    is_last_lrp: bool,
    seg: &openlr_graph::NetworkSegment,
    params: &DecodeParams,
) -> EvalResult {
    let (lon, lat) = lrp.coord;

    // Geometry oriented in the traversal direction.
    let geom: Vec<(f64, f64)> = match dir {
        TraversalDir::Forward  => seg.geometry.clone(),
        TraversalDir::Backward => seg.geometry.iter().cloned().rev().collect(),
    };

    // Project the LRP coordinate onto the segment.
    let proj = match project_onto_polyline(lon, lat, &geom) {
        Some(p) => p,
        None => return EvalResult::Rejected {
            verdict: GateVerdict::FailDirection,
            distance_m: None,
            point: None,
            bearing_deg: None,
        },
    };

    // Hard gate: search radius.
    if proj.distance_m > params.candidate_search_radius_m {
        return EvalResult::Rejected {
            verdict: GateVerdict::FailRadius {
                distance_m: proj.distance_m,
                radius_m: params.candidate_search_radius_m,
            },
            distance_m: Some(proj.distance_m),
            point: Some(proj.point),
            bearing_deg: None,
        };
    }

    // Endpoint snapping: if the projection falls within `snap_to_endpoint_threshold_m`
    // of either endpoint, move it to that endpoint exactly.
    let seg_length = seg.length_m.max(0.001); // guard against degenerate zero-length
    let threshold = params.snap_to_endpoint_threshold_m;
    let (arc_offset_m, is_at_entry, is_at_exit) = if proj.arc_offset_m < threshold {
        (0.0, true, false)
    } else if proj.arc_offset_m > seg_length - threshold {
        (seg_length, false, true)
    } else {
        (proj.arc_offset_m, false, false)
    };

    // Snapped point in (lon, lat).
    let snapped_point = if is_at_entry {
        geom.first().cloned().unwrap_or(proj.point)
    } else if is_at_exit {
        geom.last().cloned().unwrap_or(proj.point)
    } else {
        proj.point
    };

    // Bearing at the snapped arc position.
    // Non-last LRP: forward on traversal geometry = direction of travel.
    // Last LRP: backward on traversal geometry = "look back 20m toward path origin."
    //   This applies regardless of traversal direction — for Backward, the geometry
    //   is already reversed, so backward-on-reversed = forward-on-original = northward
    //   for a southbound path, which IS the correct look-back bearing.
    let forward_on_geom = !is_last_lrp;
    let bearing_deg = bearing_at_offset(&geom, arc_offset_m, forward_on_geom);

    // ── Score components (lower = better, 0 = perfect) ──────────────────────

    // Distance score: normalized to [0, 1] by the search radius.
    let distance_score = params.distance_weight
        * (proj.distance_m / params.candidate_search_radius_m);

    // Bearing score: excess outside the encoding interval, measured in buckets.
    // Bucket size = width of the LRP's own bearing interval (11.25° for v3,
    // ~1.41° for TPEG).  Treat a zero-width interval (XML) as 1°.
    let bucket_size = (lrp.bearing.ub_deg - lrp.bearing.lb_deg).max(1.0);
    let excess_deg = lrp.bearing.excess(bearing_deg);

    // Hard gate: bearing deviation beyond max_bearing_deviation_deg rejects outright.
    if excess_deg > params.max_bearing_deviation_deg {
        return EvalResult::Rejected {
            verdict: GateVerdict::FailBearing {
                excess_deg,
                max_deg: params.max_bearing_deviation_deg,
            },
            distance_m: Some(proj.distance_m),
            point: Some(snapped_point),
            bearing_deg: Some(bearing_deg),
        };
    }

    let sector_delta = excess_deg / bucket_size;
    let bearing_score = params.bearing_weight * sector_delta * params.bearing_penalty_per_bucket;

    // FRC and FOW scores from the 8×8 penalty tables.
    let frc_idx_lrp = (lrp.frc as usize).min(7);
    let frc_idx_seg = (seg.frc as usize).min(7);
    let fow_idx_lrp = (lrp.fow as usize).min(7);
    let fow_idx_seg = (seg.fow as usize).min(7);
    let frc_score = params.frc_weight * params.frc_penalty_table[frc_idx_lrp][frc_idx_seg];
    let fow_score = params.fow_weight * params.fow_penalty_table[fow_idx_lrp][fow_idx_seg];

    // Interior snap penalty: added when the LRP did not snap to either endpoint.
    let interior_score = if is_at_entry || is_at_exit {
        0.0
    } else {
        params.interior_weight
    };

    // Wrong-endpoint penalty: continuous 0→1 based on position along the segment.
    // For non-last LRPs the path enters this segment, so the correct end is entry
    //   (arc=0); being near the exit means the path must traverse the whole segment
    //   backward, which is penalized.
    // For the last LRP the path exits this segment, so the correct end is exit
    //   (arc=seg_length); being near the entry is penalized.
    let t = (arc_offset_m / seg_length).clamp(0.0, 1.0);
    let wrong_raw = if is_last_lrp { 1.0 - t } else { t };
    let wrong_endpoint_score = params.wrong_endpoint_weight * wrong_raw;

    let total = distance_score + bearing_score + frc_score + fow_score
        + interior_score + wrong_endpoint_score;

    // Hard gate: implausibly high total score.
    if total > params.max_candidate_score {
        return EvalResult::Rejected {
            verdict: GateVerdict::FailScore { total, max_score: params.max_candidate_score },
            distance_m: Some(proj.distance_m),
            point: Some(snapped_point),
            bearing_deg: Some(bearing_deg),
        };
    }

    let (entry_node, exit_node) = match dir {
        TraversalDir::Forward  => (seg.start_node, seg.end_node),
        TraversalDir::Backward => (seg.end_node,   seg.start_node),
    };

    EvalResult::Accepted(ScoredCandidate {
        segment_id: seg_id,
        traversal: dir,
        projection: ProjectionResult {
            arc_offset_m,
            point: snapped_point,
            distance_m: proj.distance_m,
            bearing_deg,
            is_at_entry,
            is_at_exit,
        },
        score: CandidateScore {
            distance_score,
            bearing_score,
            frc_score,
            fow_score,
            interior_score,
            wrong_endpoint_score,
            total,
        },
        exit_node,
        entry_node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_codec::{CircularInterval, LinearInterval};
    use openlr_codec::lrp::Lrp;
    use openlr_graph::{NetworkNode, NetworkSegment, NodeId};

    fn simple_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(NetworkNode { id: NodeId(0), lon: 0.0,   lat: 0.0,   stable_id: [0;16], is_boundary: false });
        g.add_node(NetworkNode { id: NodeId(1), lon: 0.001, lat: 0.0,   stable_id: [0;16], is_boundary: false });
        g.add_segment(NetworkSegment {
            id: SegmentId(1),
            start_node: NodeId(0),
            end_node:   NodeId(1),
            geometry: vec![(0.0, 0.0), (0.001, 0.0)],
            length_m: 100.0,
            frc: 3,
            fow: 3,
            direction: Direction::Both,
            stable_id: [0u8; 16],
        });
        g
    }

    fn lrp_near_origin(bearing_lb: f64) -> Lrp {
        Lrp {
            coord: (0.0005, 0.0001),
            bearing: CircularInterval { lb_deg: bearing_lb, ub_deg: bearing_lb + 11.25 },
            frc: 3,
            fow: 3,
            lfrcnp: Some(5),
            dnp: Some(LinearInterval { lb: 58.0, ub: 117.0 }),
            pos_offset: None,
            neg_offset: None,
        }
    }

    #[test]
    fn finds_candidate_for_eastbound_bearing() {
        let g = simple_graph();
        let lrp = lrp_near_origin(82.0); // east-ish sector
        let mut trace = DecodeTrace::new(DecodeParams::default());
        let candidates = select_candidates(0, &lrp, false, &g, &DecodeParams::default(), &mut trace);
        assert!(!candidates.is_empty(), "should find at least one candidate");
    }

    #[test]
    fn bearing_mismatch_penalized() {
        let g = simple_graph();
        // East-west segment: correct LRP faces east, slightly-off faces east+20°.
        let lrp_east    = lrp_near_origin(82.0);  // east-ish — correct for this segment
        let lrp_slight  = lrp_near_origin(40.0);  // 40° off east — within default 45° gate

        let params = DecodeParams::default();
        let mut trace_e = DecodeTrace::new(params.clone());
        let mut trace_s = DecodeTrace::new(params.clone());

        let east_cands   = select_candidates(0, &lrp_east,   false, &g, &params, &mut trace_e);
        let slight_cands = select_candidates(0, &lrp_slight, false, &g, &params, &mut trace_s);

        assert!(!east_cands.is_empty(),   "east-facing LRP should find the east-west segment");
        assert!(!slight_cands.is_empty(), "slightly-off LRP within gate should still find segment");

        // The correctly-aligned LRP should score better (lower total).
        assert!(
            east_cands[0].score.total < slight_cands[0].score.total,
            "east bearing should rank better on an east-west segment \
             (east={:.3}, slight={:.3})",
            east_cands[0].score.total,
            slight_cands[0].score.total,
        );
    }

    #[test]
    fn bearing_gate_rejects_large_deviation() {
        let g = simple_graph();
        // North-facing LRP on an east-west segment: ~82° deviation, exceeds default 45° gate.
        let lrp_north = lrp_near_origin(0.0);
        let params = DecodeParams::default(); // max_bearing_deviation_deg = 45.0
        let mut trace = DecodeTrace::new(params.clone());
        let cands = select_candidates(0, &lrp_north, false, &g, &params, &mut trace);
        assert!(cands.is_empty(), "north-facing LRP should be rejected on east-west segment");
    }

    #[test]
    fn bearing_gate_disabled_at_180() {
        let g = simple_graph();
        // Same north-facing LRP, but gate opened to 180° — should pass.
        let lrp_north = lrp_near_origin(0.0);
        let params = DecodeParams {
            max_bearing_deviation_deg: 180.0,
            max_candidate_score: 999.0,
            ..DecodeParams::default()
        };
        let mut trace = DecodeTrace::new(params.clone());
        let cands = select_candidates(0, &lrp_north, false, &g, &params, &mut trace);
        assert!(!cands.is_empty(), "gate=180° should admit all bearings");
    }
}
