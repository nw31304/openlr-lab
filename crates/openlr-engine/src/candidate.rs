use openlr_codec::lrp::Lrp;
use openlr_graph::{bearing_at_offset, haversine_m, polyline_length_m, project_onto_polyline, Direction, Graph, SegmentId};

use crate::params::DecodeParams;
use crate::trace::{
    CandidateScore, DecodeEvent, GateVerdict, ProjectionResult, RejectedCandidate,
    ScoredCandidate, SnapEvaluation, SnapType, TraversalDir, DecodeTrace,
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
                EvalResult::Accepted { candidate: scored, snap_evaluations } => {
                    trace.push_full(DecodeEvent::CandidateEvaluated {
                        lrp_idx,
                        segment_id: seg_id,
                        traversal: dir,
                        projection: scored.projection.clone(),
                        verdict: GateVerdict::Pass,
                        score: Some(scored.score.clone()),
                        snap_evaluations,
                    });
                    accepted.push(scored);
                }
                EvalResult::Rejected { verdict, distance_m, point, bearing_deg, arc_offset_m, snap_type, score, snap_evaluations } => {
                    let is_at_entry = matches!(snap_type, Some(SnapType::Entry));
                    let is_at_exit  = matches!(snap_type, Some(SnapType::Exit));
                    trace.push_full(DecodeEvent::CandidateEvaluated {
                        lrp_idx,
                        segment_id: seg_id,
                        traversal: dir,
                        projection: ProjectionResult {
                            arc_offset_m: arc_offset_m.unwrap_or(0.0),
                            point: point.unwrap_or((0.0, 0.0)),
                            distance_m: distance_m.unwrap_or(0.0),
                            bearing_deg: bearing_deg.unwrap_or(0.0),
                            is_at_entry,
                            is_at_exit,
                        },
                        verdict: verdict.clone(),
                        score: score.clone(),
                        snap_evaluations,
                    });
                    rejected.push(RejectedCandidate {
                        segment_id: seg_id,
                        traversal: dir,
                        distance_m,
                        point,
                        bearing_deg,
                        arc_offset_m,
                        verdict,
                        is_at_entry,
                        is_at_exit,
                        score,
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
    Accepted {
        candidate: ScoredCandidate,
        snap_evaluations: Vec<SnapEvaluation>,
    },
    Rejected {
        verdict: GateVerdict,
        /// Distance from LRP to projected point. `None` only for `FailDirection`.
        distance_m: Option<f64>,
        /// Snap point (lon, lat). `None` only for `FailDirection`.
        point: Option<(f64, f64)>,
        /// Bearing at the projection point. `None` for `FailDirection` and `FailRadius`.
        bearing_deg: Option<f64>,
        /// Arc offset (m) of the representative snap along the traversal-direction geometry. `None` for early rejects.
        arc_offset_m: Option<f64>,
        /// Snap type of the representative (best-gate-rank) snap. `None` for early rejects.
        snap_type: Option<SnapType>,
        /// Score of the representative snap. `None` only for `FailDirection`/`FailRadius`.
        score: Option<CandidateScore>,
        /// All individual snap evaluations performed (empty for early radius/direction rejects).
        snap_evaluations: Vec<SnapEvaluation>,
    },
}

/// One snap position to evaluate for a (segment, direction) pair.
struct SnapCandidate {
    snap_type: SnapType,
    arc_offset_m: f64,
    point: (f64, f64),
    distance_m: f64,
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

    // Project the LRP coordinate onto the segment to find the nearest point + arc offset.
    let proj = match project_onto_polyline(lon, lat, &geom) {
        Some(p) => p,
        None => return EvalResult::Rejected {
            verdict: GateVerdict::FailDirection,
            distance_m: None,
            point: None,
            bearing_deg: None,
            arc_offset_m: None,
            snap_type: None,
            score: None,
            snap_evaluations: vec![],
        },
    };

    // Hard gate: if the nearest point on the segment is beyond the search radius,
    // no endpoint can be closer (interior projection gives the global minimum distance).
    if proj.distance_m > params.candidate_search_radius_m {
        return EvalResult::Rejected {
            verdict: GateVerdict::FailRadius {
                distance_m: proj.distance_m,
                radius_m: params.candidate_search_radius_m,
            },
            distance_m: Some(proj.distance_m),
            point: Some(proj.point),
            bearing_deg: None,
            arc_offset_m: None,
            snap_type: None,
            score: None,
            snap_evaluations: vec![],
        };
    }

    // ── Build the list of snap positions to evaluate ──────────────────────────
    //
    // Use the ACTUAL polyline arc length rather than seg.length_m: the stored
    // precomputed length can differ slightly from what project_onto_polyline
    // computes, causing arc_offset_m to appear interior when it is really at the
    // exit endpoint (or vice versa).  polyline_length_m(&geom) is authoritative.
    let seg_length = polyline_length_m(&geom).max(0.001);
    let threshold = params.snap_to_endpoint_threshold_m;
    let arc = proj.arc_offset_m;

    let mut snaps: Vec<SnapCandidate> = Vec::with_capacity(3);

    // Interior: only when the foot falls strictly inside the segment AND outside both
    // endpoint threshold zones.  If the projection lands within `threshold` of either
    // endpoint we exclusively offer that endpoint snap so the correct penalty (none,
    // wrong_endpoint, or interior) is applied — never let a near-endpoint interior snap
    // dodge the wrong_endpoint charge by winning on a marginally smaller distance.
    if arc > 1e-6 && arc < seg_length - 1e-6 && arc > threshold && seg_length - arc > threshold {
        snaps.push(SnapCandidate {
            snap_type: SnapType::Interior,
            arc_offset_m: arc,
            point: proj.point,
            distance_m: proj.distance_m,
        });
    }

    // Entry endpoint: arc distance from projection to entry is `arc`.
    if arc <= threshold {
        let entry = *geom.first().expect("geom has ≥2 vertices");
        snaps.push(SnapCandidate {
            snap_type: SnapType::Entry,
            arc_offset_m: 0.0,
            point: entry,
            distance_m: haversine_m(lon, lat, entry.0, entry.1),
        });
    }

    // Exit endpoint: arc distance from projection to exit is `seg_length - arc`.
    if seg_length - arc <= threshold {
        let exit = *geom.last().expect("geom has ≥2 vertices");
        snaps.push(SnapCandidate {
            snap_type: SnapType::Exit,
            arc_offset_m: seg_length,
            point: exit,
            distance_m: haversine_m(lon, lat, exit.0, exit.1),
        });
    }

    // Degenerate guard (e.g. extremely short segment where arc lands at 0 or seg_length
    // exactly and threshold is 0): always have at least one snap to evaluate.
    if snaps.is_empty() {
        let (st, snap_arc, pt) = if arc <= seg_length / 2.0 {
            (SnapType::Entry, 0.0, *geom.first().expect("geom has ≥2 vertices"))
        } else {
            (SnapType::Exit, seg_length, *geom.last().expect("geom has ≥2 vertices"))
        };
        snaps.push(SnapCandidate {
            snap_type: st,
            arc_offset_m: snap_arc,
            point: pt,
            distance_m: haversine_m(lon, lat, pt.0, pt.1),
        });
    }

    // ── Score each snap position ──────────────────────────────────────────────

    let forward_on_geom = !is_last_lrp;
    let bucket_size = (lrp.bearing.ub_deg - lrp.bearing.lb_deg).max(1.0);
    let frc_idx_lrp = (lrp.frc as usize).min(7);
    let frc_idx_seg = (seg.frc as usize).min(7);
    let fow_idx_lrp = (lrp.fow as usize).min(7);
    let fow_idx_seg = (seg.fow as usize).min(7);
    let frc_score = params.frc_weight * params.frc_penalty_table[frc_idx_lrp][frc_idx_seg];
    let fow_score = params.fow_weight * params.fow_penalty_table[fow_idx_lrp][fow_idx_seg];

    let mut snap_evals: Vec<SnapEvaluation> = Vec::with_capacity(snaps.len());
    let mut best: Option<(f64, usize)> = None; // (total_score, index into snap_evals)

    for snap in snaps {
        let bearing_deg = bearing_at_offset(&geom, snap.arc_offset_m, forward_on_geom);
        let excess_deg  = lrp.bearing.excess(bearing_deg);

        let (is_at_entry, is_at_exit) = match snap.snap_type {
            SnapType::Interior => (false, false),
            SnapType::Entry    => (true,  false),
            SnapType::Exit     => (false, true),
        };

        // Compute all score components unconditionally — scores are diagnostic even
        // for rejected snaps, and are needed by the UI to explain why a candidate lost.
        let distance_score = params.distance_weight
            * (snap.distance_m / params.candidate_search_radius_m);
        let sector_delta         = excess_deg / bucket_size;
        let bearing_score        = params.bearing_weight * sector_delta * params.bearing_penalty_per_bucket;
        let interior_score       = if is_at_entry || is_at_exit { 0.0 } else { params.interior_weight };
        let wrong_raw            = match (is_last_lrp, is_at_entry, is_at_exit) {
            (false, false, true) => 1.0, // non-last: entry is correct, exit is wrong
            (true,  true, false) => 1.0, // last: exit is correct, entry is wrong
            _                    => 0.0,
        };
        let wrong_endpoint_score = params.wrong_endpoint_weight * wrong_raw;
        let total = distance_score + bearing_score + frc_score + fow_score
            + interior_score + wrong_endpoint_score;

        let score = CandidateScore {
            distance_score, bearing_score, frc_score, fow_score,
            interior_score, wrong_endpoint_score, total,
        };

        // Hard gate: bearing deviation.
        if excess_deg > params.max_bearing_deviation_deg {
            snap_evals.push(SnapEvaluation {
                snap_type: snap.snap_type,
                arc_offset_m: snap.arc_offset_m,
                point: snap.point,
                distance_m: snap.distance_m,
                bearing_deg,
                score: Some(score),
                verdict: GateVerdict::FailBearing {
                    excess_deg,
                    max_deg: params.max_bearing_deviation_deg,
                },
            });
            continue;
        }

        // Hard gate: implausibly high total score.
        if total > params.max_candidate_score {
            snap_evals.push(SnapEvaluation {
                snap_type: snap.snap_type,
                arc_offset_m: snap.arc_offset_m,
                point: snap.point,
                distance_m: snap.distance_m,
                bearing_deg,
                score: Some(score),
                verdict: GateVerdict::FailScore { total, max_score: params.max_candidate_score },
            });
            continue;
        }

        let idx = snap_evals.len();
        snap_evals.push(SnapEvaluation {
            snap_type: snap.snap_type,
            arc_offset_m: snap.arc_offset_m,
            point: snap.point,
            distance_m: snap.distance_m,
            bearing_deg,
            score: Some(score),
            verdict: GateVerdict::Pass,
        });

        if best.as_ref().map_or(true, |&(bt, _)| total < bt) {
            best = Some((total, idx));
        }
    }

    // ── No snap passed all gates — reject ────────────────────────────────────

    let Some((_, best_idx)) = best else {
        // For summary reporting, pick the snap that got furthest through the gate
        // sequence. Prefer a FailScore over FailBearing over (no snap at all).
        let rep = snap_evals
            .iter()
            .max_by_key(|e| gate_rank(&e.verdict))
            .or(snap_evals.first());
        let verdict      = rep.map_or(GateVerdict::FailDirection, |e| e.verdict.clone());
        let distance_m   = rep.map(|e| e.distance_m);
        let point        = rep.map(|e| e.point);
        let arc_offset_m = rep.map(|e| e.arc_offset_m);
        let snap_type    = rep.map(|e| e.snap_type.clone());
        let score        = rep.and_then(|e| e.score.clone());
        let bearing_deg  = rep.and_then(|e| match &e.verdict {
            GateVerdict::FailBearing { .. } | GateVerdict::FailScore { .. } => Some(e.bearing_deg),
            _ => None,
        });
        return EvalResult::Rejected { verdict, distance_m, point, bearing_deg, arc_offset_m, snap_type, score, snap_evaluations: snap_evals };
    };

    // ── Best snap wins ────────────────────────────────────────────────────────

    let winner = &snap_evals[best_idx];
    let winner_score = winner.score.clone().unwrap(); // safe: winner passed all gates

    let (entry_node, exit_node) = match dir {
        TraversalDir::Forward  => (seg.start_node, seg.end_node),
        TraversalDir::Backward => (seg.end_node,   seg.start_node),
    };

    EvalResult::Accepted {
        candidate: ScoredCandidate {
            segment_id: seg_id,
            traversal: dir,
            projection: ProjectionResult {
                arc_offset_m:  winner.arc_offset_m,
                point:         winner.point,
                distance_m:    winner.distance_m,
                bearing_deg:   winner.bearing_deg,
                is_at_entry:   matches!(winner.snap_type, SnapType::Entry),
                is_at_exit:    matches!(winner.snap_type, SnapType::Exit),
            },
            score: winner_score,
            exit_node,
            entry_node,
        },
        snap_evaluations: snap_evals,
    }
}

/// Ordinal for gate ordering: higher = got further through the gate sequence.
fn gate_rank(v: &GateVerdict) -> u8 {
    match v {
        GateVerdict::FailDirection  => 0,
        GateVerdict::FailRadius { .. } => 1,
        GateVerdict::FailBearing { .. } => 2,
        GateVerdict::FailScore { .. }   => 3,
        GateVerdict::Pass               => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_codec::{CircularInterval, LinearInterval};
    use openlr_codec::lrp::Lrp;
    use openlr_graph::{NetworkNode, NetworkSegment, NodeId};

    fn simple_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(NetworkNode { id: NodeId(0), lon: 0.0,   lat: 0.0,   stable_id: String::new(), is_boundary: false });
        g.add_node(NetworkNode { id: NodeId(1), lon: 0.001, lat: 0.0,   stable_id: String::new(), is_boundary: false });
        g.add_segment(NetworkSegment {
            id: SegmentId(1),
            start_node: NodeId(0),
            end_node:   NodeId(1),
            geometry: vec![(0.0, 0.0), (0.001, 0.0)],
            length_m: 100.0,
            frc: 3,
            fow: 3,
            direction: Direction::Both,
            stable_id: String::new(),
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
            pos_offset: None, neg_offset: None,
            pos_offset_raw: None, neg_offset_raw: None,
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
