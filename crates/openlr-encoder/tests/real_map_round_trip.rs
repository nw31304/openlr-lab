//! End-to-end validation: encode a real path from an actual map archive, then
//! decode the produced reference with the real engine, and confirm the
//! reconstructed path matches. This is the spec's own correctness invariant
//! (shortest-path-between-LRPs reproduces the original path), exercised
//! against genuine topology rather than a synthetic graph.
//!
//! Skips gracefully (prints a message, doesn't fail) when no pmtiles archive
//! is present in this checkout — mirrors openlr-provider's own integration
//! tests, which have the same requirement.

use std::path::PathBuf;

use openlr_encoder::line::{encode_line, LineLocationInput};
use openlr_engine::{decode, DecodeParams, Preset};
use openlr_graph::{Direction, NodeId, SegmentId, TileKey};
use openlr_provider::PmtilesProvider;

/// True if `needle` appears as a contiguous, in-order run within `haystack`.
/// The decoded path can legitimately be longer than the originally-encoded
/// path when the encoder had to expand outward to reach valid junction nodes
/// (Rule-4) — the correctness invariant is "contains the original route
/// intact", not literal equality; the expansion overhang is exactly what
/// POFF/NOFF account for.
fn contains_subsequence(haystack: &[SegmentId], needle: &[SegmentId]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2) // crates/openlr-encoder → workspace root
        .unwrap()
        .to_path_buf()
}

fn any_archive() -> Option<PathBuf> {
    // Full-scale Europe archive used for real-map testing (see project notes).
    let absolute = PathBuf::from("/Users/dave/projects/maps/pmtiles/eur-osm/openlrlens-world-europe-latest.pmtiles");
    if absolute.exists() {
        return Some(absolute);
    }
    let candidates = [
        "out/nz-osm/openlrlens-nz-new-zealand-latest.pmtiles",
        "out/de-osm/openlrlens-de-germany-latest.pmtiles",
        "out/openlrlens-de-germany-latest.pmtiles",
        "out/qatar/openlrlens-qatar-generic.pmtiles",
    ];
    let ws = workspace_root();
    candidates.iter().map(|c| ws.join(c)).find(|p| p.exists())
}

/// Find a real, genuinely-shortest multi-segment path: greedily walk forward
/// `hops` steps to discover *some* reachable end point (this walk itself need
/// not be shortest — it's just a way to find two real, distant points), then
/// hand that (start, end) pair to the actual `shortest_path` primitive. The
/// result is guaranteed encodable in the same way a real router's output
/// would be — unlike the raw greedy walk itself, which frequently isn't the
/// shortest route in dense urban topology (confirmed empirically: attempts
/// with a naive greedy path here regularly hit `NoRoute`, exactly as expected
/// per Dijkstra's optimal-substructure property discussed in coverage.rs).
fn find_real_path(graph: &openlr_graph::Graph, hops: usize) -> Option<(NodeId, Vec<SegmentId>)> {
    'outer: for start_seg in graph.segments.values() {
        if start_seg.length_m < 20.0 || matches!(start_seg.direction, Direction::Backward) {
            continue;
        }
        let start_node = start_seg.start_node;
        let mut node = start_seg.end_node;
        let mut incoming = start_seg.id;

        for _ in 0..hops - 1 {
            let next = graph.successors(node, incoming, 7, 180.0)
                .into_iter()
                .find(|(_, seg_id, len)| *len >= 20.0 && !graph.is_restricted(incoming, node, *seg_id));
            match next {
                Some((next_node, next_seg, _)) => {
                    node = next_node;
                    incoming = next_seg;
                }
                None => continue 'outer,
            }
        }
        if node == start_node {
            continue;
        }
        if let openlr_graph::PathOutcome::Found(result) = openlr_graph::shortest_path(graph, start_node, openlr_graph::NO_PRIOR_SEG, node, 7, 180.0, 0, 12) {
            if result.segments.len() >= 2 {
                return Some((start_node, result.segments));
            }
        }
    }
    None
}

#[test]
fn encode_then_decode_real_path_round_trips() {
    let Some(archive) = any_archive() else {
        eprintln!("SKIP: no pmtiles archive found in out/ — build one with openlr-pmtiles to run this test");
        return;
    };

    let Ok(mut provider) = PmtilesProvider::open(&archive) else {
        eprintln!("SKIP: failed to open {archive:?} — likely a stale/incompatible fixture");
        return;
    };
    // Try a few well-known urban centers; harmless if a given one isn't
    // covered by this particular archive — load_tiles just finds nothing (or,
    // for a stale/incompatible fixture, errors, which we also just skip past).
    for (lon, lat) in [(174.76, -36.85), (13.405, 52.52), (51.53, 25.29)] {
        let key = TileKey::from_lonlat(lon, lat, 12);
        match provider.load_tiles(&key.neighborhood()) {
            Ok(()) => eprintln!("loaded ({lon},{lat}): {} segments so far", provider.graph().segments.len()),
            Err(e) => eprintln!("load_tiles({lon},{lat}) failed: {e}"),
        }
        if !provider.graph().segments.is_empty() {
            break;
        }
    }

    let graph = provider.graph();
    if graph.segments.is_empty() {
        eprintln!("SKIP: no segments loaded for any test region — archive covers none, or is incompatible");
        return;
    }

    let (start_node, path) = find_real_path(graph, 3)
        .expect("should find at least one 3-segment connected path in a real urban tile");

    let total_len_m: f64 = path.iter().filter_map(|id| graph.segments.get(id)).map(|s| s.length_m).sum();
    eprintln!("Encoding a real {}-segment path ({total_len_m:.1}m) starting at {:?}", path.len(), start_node);

    let input = LineLocationInput {
        path: path.clone(),
        start_node,
        start_offset_m: 0.0,
        end_offset_m: 0.0,
        via_split_points: vec![],
    };
    let loc_ref = encode_line(graph, &input, 150.0, 15_000.0, 12).expect("encoding failed");
    for (i, lrp) in loc_ref.lrps().unwrap().iter().enumerate() {
        eprintln!(
            "LRP{i}: coord={:?} frc={} fow={} lfrcnp={:?} dnp={:?} pos_off={:?} neg_off={:?}",
            lrp.coord, lrp.frc, lrp.fow, lrp.lfrcnp,
            lrp.dnp.map(|d| d.lb), lrp.pos_offset.map(|d| d.lb), lrp.neg_offset.map(|d| d.lb),
        );
    }

    let v3 = openlr_codec::encode_v3_base64(&loc_ref).expect("v3 serialization failed");
    let tpeg = openlr_codec::encode_tpeg_hex(&loc_ref).expect("tpeg serialization failed");
    eprintln!("v3:   {v3}");
    eprintln!("tpeg: {tpeg}");

    // Decode the v3 reference with the *real* engine and confirm the path matches.
    let redecoded = openlr_codec::decode_v3_base64(&v3).expect("v3 decode failed");
    let params = DecodeParams::preset(Preset::Permissive);
    let result = decode(&redecoded, graph, &params, 12).expect("engine decode failed");
    let decoded_path = result.as_network().expect("expected a network location").path.clone();

    eprintln!("original path:  {path:?}");
    eprintln!("decoded path:   {decoded_path:?}");
    assert!(
        contains_subsequence(&decoded_path, &path),
        "decoded path should contain the originally encoded path intact (possibly with \
         valid-node expansion overhang on either end, accounted for by POFF/NOFF)"
    );

    // Same check for TPEG.
    let redecoded_tpeg = openlr_codec::decode_tpeg_hex(&tpeg).expect("tpeg decode failed");
    let result_tpeg = decode(&redecoded_tpeg, graph, &params, 12).expect("engine decode (tpeg) failed");
    let decoded_path_tpeg = result_tpeg.as_network().expect("expected a network location").path.clone();
    assert!(
        contains_subsequence(&decoded_path_tpeg, &path),
        "TPEG round trip should also contain the originally encoded path intact"
    );
}

/// Deterministic diagnostic scan (not part of the regular suite): iterates
/// start segments in sorted-ID order (unlike the random-order default test)
/// so failures are reproducible run-to-run, and reports every distinct
/// failure category with the exact (start_seg_id, hops) seed that caused it.
///
/// Note: a "DIVERGED" report here can be a false positive rather than a real
/// bug — real map data sometimes has two genuinely parallel/duplicate
/// segments between the same two nodes (e.g. a divided carriageway or a
/// duplicated OSM way). When that happens, `contains_subsequence`'s exact
/// segment-ID match correctly flags a difference even though both routes are
/// equally valid and equal-cost. Confirm any reported divergence isn't this
/// before treating it as a real issue (check whether the two differing
/// segment IDs share the same start/end nodes and length).
#[test]
#[ignore]
fn scan_for_edge_cases() {
    let Some(archive) = any_archive() else { return; };
    let Ok(mut provider) = PmtilesProvider::open(&archive) else { return; };
    // Much bigger than the default 3x3 neighborhood, to rule out "path search
    // hit the edge of loaded tiles" as an explanation for NoRoute/divergence.
    let center = TileKey::from_lonlat(13.405, 52.52, 12);
    let mut big_area = Vec::new();
    for dy in -8i32..=8 {
        for dx in -8i32..=8 {
            let x = center.x as i32 + dx;
            let y = center.y as i32 + dy;
            if x >= 0 && y >= 0 {
                big_area.push(TileKey { z: 12, x: x as u32, y: y as u32 });
            }
        }
    }
    provider.load_tiles(&big_area).expect("bulk tile load failed");
    let graph = provider.graph();
    eprintln!("Loaded {} tiles -> {} segments", big_area.len(), graph.segments.len());
    if graph.segments.is_empty() { return; }

    let mut seg_ids: Vec<SegmentId> = graph.segments.keys().copied().collect();
    seg_ids.sort_by_key(|s| s.0);

    let params = DecodeParams::preset(Preset::Permissive);
    let mut no_route = 0;
    let mut other_encode_err = 0;
    let mut decode_err = 0;
    let mut diverged = 0;
    let mut ok = 0;

    for hops in [5usize, 10, 15, 20] {
        for &start_seg_id in &seg_ids {
            let start_seg = &graph.segments[&start_seg_id];
            if start_seg.length_m < 20.0 || matches!(start_seg.direction, Direction::Backward) {
                continue;
            }
            let start_node = start_seg.start_node;
            let mut node = start_seg.end_node;
            let mut incoming = start_seg.id;
            let mut ok_walk = true;
            for _ in 0..hops - 1 {
                match graph.successors(node, incoming, 7, 180.0).into_iter()
                    .find(|(_, seg_id, len)| *len >= 20.0 && !graph.is_restricted(incoming, node, *seg_id))
                {
                    Some((n, s, _)) => { node = n; incoming = s; }
                    None => { ok_walk = false; break; }
                }
            }
            if !ok_walk || node == start_node { continue; }
            let openlr_graph::PathOutcome::Found(sp) = openlr_graph::shortest_path(graph, start_node, openlr_graph::NO_PRIOR_SEG, node, 7, 180.0, 0, 12) else { continue };
            if sp.segments.len() < 2 { continue; }
            let path = sp.segments;

            let input = LineLocationInput { path: path.clone(), start_node, start_offset_m: 0.0, end_offset_m: 0.0, via_split_points: vec![] };
            match encode_line(graph, &input, 180.0, 15_000.0, 12) {
                Err(openlr_encoder::EncodeError::NoRoute) => { no_route += 1; continue; }
                Err(e) => { other_encode_err += 1; eprintln!("OTHER ENCODE ERR seed=({start_seg_id:?},{hops}): {e}"); continue; }
                Ok(loc_ref) => {
                    let v3 = openlr_codec::encode_v3_base64(&loc_ref).unwrap();
                    let redecoded = openlr_codec::decode_v3_base64(&v3).unwrap();
                    match decode(&redecoded, graph, &params, 12) {
                        Err(e) => { decode_err += 1; eprintln!("DECODE ERR seed=({start_seg_id:?},{hops}): {e}"); }
                        Ok(result) => {
                            let decoded_path = result.as_network().unwrap().path.clone();
                            if contains_subsequence(&decoded_path, &path) {
                                ok += 1;
                            } else {
                                diverged += 1;
                                eprintln!("DIVERGED seed=({start_seg_id:?},{hops})");
                                eprintln!("  original: {path:?}");
                                eprintln!("  decoded:  {decoded_path:?}");
                                for (i, lrp) in loc_ref.lrps().unwrap().iter().enumerate() {
                                    eprintln!("  LRP{i}: coord={:?} frc={} fow={} bearing=[{:.1},{:.1}] dnp={:?} pos_off={:?} neg_off={:?}",
                                        lrp.coord, lrp.frc, lrp.fow, lrp.bearing.lb_deg, lrp.bearing.ub_deg,
                                        lrp.dnp.map(|d| d.lb), lrp.pos_offset.map(|d| d.lb), lrp.neg_offset.map(|d| d.lb));
                                }
                                if diverged >= 3 {
                                    eprintln!("Stopping early after 3 diverged cases.");
                                    eprintln!("SUMMARY: ok={ok} no_route={no_route} other_encode_err={other_encode_err} decode_err={decode_err} diverged={diverged}");
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    eprintln!("SUMMARY: ok={ok} no_route={no_route} other_encode_err={other_encode_err} decode_err={decode_err} diverged={diverged}");
}

/// Regression check for the original motivating case behind the `try
/// top-scored combination first` precheck fix — a real Germany 2-LRP leg
/// (LRP4->LRP5 of a known 6-LRP reference) that specifically needed the
/// existing same-segment/adjacent precheck to decode correctly. Confirms the
/// fix (trying the all-top-candidate combination before that scan) doesn't
/// regress it.
#[test]
#[ignore]
fn germany_leg4_regression_check() {
    let Some(archive) = any_archive() else { return; };
    let Ok(mut provider) = PmtilesProvider::open(&archive) else { return; };

    use openlr_codec::decoder::v3::decode_v3_base64;
    use openlr_codec::lrp::LocationReference;
    use openlr_engine::prefetch_tile_keys;

    let full_ref = decode_v3_base64("CwV/ECHkoiORC//N/bIjjRYD+fy+I44FAAv+0yOOAwAL/2cbcn3flfluGwM=")
        .expect("v3 decode failed");
    let lrp4 = full_ref.lrps().unwrap()[4].clone();
    let lrp5 = full_ref.lrps().unwrap()[5].clone();
    let loc_ref = LocationReference::Line { lrps: vec![lrp4, lrp5] };

    let params = DecodeParams::preset(Preset::Permissive);
    let keys = prefetch_tile_keys(loc_ref.lrps().unwrap(), &params, 12);
    eprintln!("Prefetching {} tile(s)", keys.len());
    provider.load_tiles(&keys).expect("tile load failed");
    eprintln!("Graph: {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());
    if provider.graph().segments.is_empty() {
        eprintln!("SKIP: this archive doesn't cover the Germany test area");
        return;
    }

    // Real callers must retry on NeedsTile by loading the requested tile and
    // decoding again — A* can require tiles beyond the initial LRP-coordinate
    // prefetch when the true route runs close to a tile boundary.
    let mut attempts = 0;
    let result = loop {
        match decode(&loc_ref, provider.graph(), &params, 12) {
            Err(openlr_engine::DecodeFailure { error: openlr_engine::DecodeError::NeedsTile(tk), .. }) if attempts < 10 => {
                attempts += 1;
                eprintln!("loading additional tile {tk:?} (attempt {attempts})");
                provider.load_tiles(&[tk]).expect("additional tile load failed");
            }
            other => break other,
        }
    };
    match result {
        Ok(decoded) => {
            let decoded = decoded.as_network().expect("expected network-based location");
            eprintln!("Leg-4 decode OK: {} segment(s): {:?}", decoded.path.len(), decoded.path);
        }
        Err(e) => panic!("Leg-4 decode FAILED (regression!): {e}"),
    }
}
