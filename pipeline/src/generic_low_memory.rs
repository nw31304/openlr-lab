/// DuckDB-backed GeoJSONL pipeline, activated when `--low-memory` is passed
/// with `--roads` input.
///
/// Unlike the OSM path there is no two-pass scan: every GeoJSONL line contains
/// its own geometry, attributes, and node IDs, so extract + quantize are a
/// single pass.  Restrictions from the optional CSV are loaded afterward.
/// Tiling reuses the shared `lowmem_tile::tile_from_duckdb`, unchanged.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use flate2::read::GzDecoder;
use serde_json::Value;
use tracing::{info, warn};

use crate::lowmem_tile::{
    geom_to_blob, make_bar, make_spinner, remove_collinear_lm, tile_from_duckdb,
};
use crate::partition::available_ram_bytes;
use crate::split::haversine_m;
use crate::tile::{lon_lat_to_tile_xy, node_stable_id_str, seg_stable_id_str};

// ── ID encoding (mirrors generic_extract.rs, private here) ───────────────────

fn segment_id_bytes(id: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&(id as u64).to_le_bytes());
    b
}

fn node_id_bytes(id: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&(id as u64).to_le_bytes());
    b
}

// ── DuckDB setup ──────────────────────────────────────────────────────────────

fn setup_duckdb(memory_mb_override: Option<u64>, temp_dir: &Path) -> Result<Connection> {
    let limit_mb = match memory_mb_override {
        Some(mb) => mb,
        None => {
            let avail = available_ram_bytes();
            ((avail as f64 * 0.40) / 1_048_576.0) as u64
        }
    }.max(1_024);

    std::fs::create_dir_all(temp_dir).context("create DuckDB temp dir")?;
    let db_file = temp_dir.join("pipeline.duckdb");
    let conn = Connection::open(&db_file).context("open DuckDB")?;
    conn.execute_batch(&format!(
        "PRAGMA threads={threads}; \
         SET memory_limit='{limit_mb}MB'; \
         SET preserve_insertion_order=false; \
         CREATE TABLE q_edges ( \
             edge_idx INTEGER, split_idx INTEGER, \
             start_id BLOB, end_id BLOB, parent_id BLOB, \
             geom_blob BLOB, length_cm INTEGER, \
             frc INTEGER, fow INTEGER, direction INTEGER, \
             tile_x INTEGER, tile_y INTEGER, tile_id BIGINT, stable_id VARCHAR); \
         CREATE TABLE q_nodes (node_id BLOB, lon_e7 INTEGER, lat_e7 INTEGER, tile_x INTEGER, tile_y INTEGER, stable_id VARCHAR); \
         CREATE TABLE restriction_triples (from_id BLOB, via_id BLOB, to_id BLOB, flags INTEGER);",
        threads = rayon::current_num_threads().min(8),
    ))
    .context("DuckDB schema")?;
    info!(limit_mb, "DuckDB scratch database ready");
    Ok(conn)
}

// ── Edge batch ────────────────────────────────────────────────────────────────

struct EdgeBatch {
    start_id: [u8; 16],
    end_id:   [u8; 16],
    parent_id:[u8; 16],
    geom:       Vec<(i32, i32)>,    // quantized (lon_e7, lat_e7) vertices
    length_cm:  u32,
    frc:        u8,
    fow:        u8,
    direction:  u8,
}

// ── Feature parsing ───────────────────────────────────────────────────────────

fn map_flowdir(flowdir: i64) -> u8 {
    match flowdir {
        2 => 2, // Backward
        3 => 3, // Forward
        _ => 1, // Both (1 = bidirectional; anything unknown → Both)
    }
}

/// Parse one GeoJSONL line and quantize its geometry immediately.
/// Returns None for blank lines or degenerate geometry (< 2 vertices).
fn parse_line(
    line: &str,
    seg_to_to_int: &mut std::collections::HashMap<i64, i64>,
) -> Result<Option<EdgeBatch>> {
    let v: Value = serde_json::from_str(line).context("JSON parse")?;

    let (props, geom_val) = if let Some(p) = v.get("properties") {
        let g = v.get("geometry").context("missing geometry")?;
        (p, g)
    } else {
        let g = v.get("geometry").context("missing geometry")?;
        (&v, g)
    };

    let id       = props.get("id")      .and_then(Value::as_i64).context("missing id")?;
    let frc_raw  = props.get("frc")     .and_then(Value::as_i64).context("missing frc")?;
    let fow_raw  = props.get("fow")     .and_then(Value::as_i64).context("missing fow")?;
    let flowdir  = props.get("flowdir") .and_then(Value::as_i64).context("missing flowdir")?;
    let from_int = props.get("from_int").and_then(Value::as_i64).context("missing from_int")?;
    let to_int   = props.get("to_int")  .and_then(Value::as_i64).context("missing to_int")?;

    let coords = geom_val
        .get("coordinates")
        .and_then(Value::as_array)
        .context("missing coordinates")?;

    if coords.len() < 2 {
        return Ok(None);
    }

    let mut float_geom: Vec<(f64, f64)> = Vec::with_capacity(coords.len());
    for (i, c) in coords.iter().enumerate() {
        let arr = c.as_array()
            .with_context(|| format!("coordinate[{i}] not array"))?;
        let lon = arr.first().and_then(Value::as_f64)
            .with_context(|| format!("coordinate[{i}] missing lon"))?;
        let lat = arr.get(1).and_then(Value::as_f64)
            .with_context(|| format!("coordinate[{i}] missing lat"))?;
        float_geom.push((lon, lat));
    }

    let length_m: f64 = float_geom
        .windows(2)
        .map(|w| haversine_m(w[0].0, w[0].1, w[1].0, w[1].1))
        .sum();
    let length_cm = (length_m * 100.0).round() as u32;

    // Quantize to 1e-7 degree integers (sub-meter, Invariant 4).
    let q_geom_raw: Vec<(i32, i32)> = float_geom.iter()
        .map(|&(lon, lat)| (
            (lon * 1e7).round() as i32,
            (lat * 1e7).round() as i32,
        ))
        .collect();
    let geom = remove_collinear_lm(q_geom_raw);
    if geom.len() < 2 {
        return Ok(None);
    }

    seg_to_to_int.insert(id, to_int);

    Ok(Some(EdgeBatch {
        start_id:  node_id_bytes(from_int),
        end_id:    node_id_bytes(to_int),
        parent_id: segment_id_bytes(id),
        geom,
        length_cm,
        frc:       frc_raw.clamp(0, 7) as u8,
        fow:       fow_raw.clamp(0, 7) as u8,
        direction: map_flowdir(flowdir),
    }))
}

// ── Phase 1: Extract + quantize from GeoJSONL ─────────────────────────────────

fn process_geojsonl_file(
    path: &Path,
    tile_zoom: u8,
    seg_to_to_int: &mut std::collections::HashMap<i64, i64>,
    seen_nodes: &mut HashSet<[u8; 16]>,
    edge_idx: &mut u32,
    edge_app: &mut duckdb::Appender<'_>,
    node_app: &mut duckdb::Appender<'_>,
    pb: &indicatif::ProgressBar,
) -> Result<()> {
    let file = File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let path_str = path.to_string_lossy().to_lowercase();
    let reader: Box<dyn BufRead> = if path_str.ends_with(".gz") {
        Box::new(BufReader::new(GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };

    let mut n_skip = 0usize;
    for (line_no, line_result) in reader.lines().enumerate() {
        let line = line_result
            .with_context(|| format!("read line {} of {}", line_no + 1, path.display()))?;
        let line = line.trim();
        if line.is_empty() { continue; }

        match parse_line(line, seg_to_to_int) {
            Ok(Some(e)) => {
                let (slon_e7, slat_e7) = e.geom[0];
                let (elon_e7, elat_e7) = *e.geom.last().unwrap();

                if seen_nodes.insert(e.start_id) {
                    let (tx, ty) = lon_lat_to_tile_xy(
                        slon_e7 as f64 / 1e7, slat_e7 as f64 / 1e7, tile_zoom,
                    );
                    let stable_id = node_stable_id_str(&e.start_id);
                    node_app.append_row(params![
                        &e.start_id[..], slon_e7, slat_e7, tx as i64, ty as i64, stable_id.as_str(),
                    ]).context("append q_node")?;
                }
                if seen_nodes.insert(e.end_id) {
                    let (tx, ty) = lon_lat_to_tile_xy(
                        elon_e7 as f64 / 1e7, elat_e7 as f64 / 1e7, tile_zoom,
                    );
                    let stable_id = node_stable_id_str(&e.end_id);
                    node_app.append_row(params![
                        &e.end_id[..], elon_e7, elat_e7, tx as i64, ty as i64, stable_id.as_str(),
                    ]).context("append q_node")?;
                }

                let (stx, sty) = lon_lat_to_tile_xy(
                    slon_e7 as f64 / 1e7, slat_e7 as f64 / 1e7, tile_zoom,
                );
                let (etx, ety) = lon_lat_to_tile_xy(
                    elon_e7 as f64 / 1e7, elat_e7 as f64 / 1e7, tile_zoom,
                );
                let geom_blob = geom_to_blob(&e.geom);
                // GeoJSONL input is already node-to-node (Invariant 1 satisfied by the
                // producer), so every line is its own final edge: split_idx is always 0.
                let edge_stable_id = seg_stable_id_str(&e.parent_id, 0);
                edge_app.append_row(params![
                    *edge_idx as i64, 0i64,
                    &e.start_id[..], &e.end_id[..], &e.parent_id[..],
                    geom_blob.as_slice(), e.length_cm as i64,
                    e.frc as i64, e.fow as i64, e.direction as i64,
                    stx as i64, sty as i64,
                    crate::tile::xyz_to_tile_id(tile_zoom, stx, sty) as i64,
                    edge_stable_id.as_str(),
                ]).context("append q_edge start-tile")?;
                if (etx, ety) != (stx, sty) {
                    edge_app.append_row(params![
                        *edge_idx as i64, 0i64,
                        &e.start_id[..], &e.end_id[..], &e.parent_id[..],
                        geom_blob.as_slice(), e.length_cm as i64,
                        e.frc as i64, e.fow as i64, e.direction as i64,
                        etx as i64, ety as i64,
                        crate::tile::xyz_to_tile_id(tile_zoom, etx, ety) as i64,
                        edge_stable_id.as_str(),
                    ]).context("append q_edge end-tile")?;
                }
                *edge_idx += 1;
                pb.inc(1);
            }
            Ok(None) => { n_skip += 1; }
            Err(err) => {
                warn!(path = %path.display(), line = line_no + 1, error = %err, "parse error, skipped");
                n_skip += 1;
            }
        }
    }
    if n_skip > 0 {
        warn!(path = %path.display(), n_skip, "lines skipped");
    }
    Ok(())
}

/// Single-pass extract: reads GeoJSONL lines, quantizes in-place, inserts to
/// `q_edges` and `q_nodes`.  Returns `seg_to_to_int` for restriction loading.
fn extract_quantize(
    roads_path: &Path,
    conn: &Connection,
    tile_zoom: u8,
    show_progress: bool,
) -> Result<std::collections::HashMap<i64, i64>> {
    let mut seg_to_to_int = std::collections::HashMap::new();
    let mut seen_nodes: HashSet<[u8; 16]> = HashSet::new();
    let mut edge_idx: u32 = 0;

    let mut edge_app = conn.appender("q_edges").context("appender q_edges")?;
    let mut node_app = conn.appender("q_nodes").context("appender q_nodes")?;

    let pb = make_spinner(show_progress, "Extracting GeoJSONL     ");

    if roads_path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(roads_path)
            .with_context(|| format!("read dir {}", roads_path.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let s = p.to_string_lossy().to_lowercase();
                s.ends_with(".geojsonl") || s.ends_with(".geojsonl.gz")
                    || s.ends_with(".geojson") || s.ends_with(".geojson.gz")
            })
            .collect();
        entries.sort();
        let bar = make_bar(show_progress, entries.len() as u64, "GeoJSONL files          ");
        for path in &entries {
            process_geojsonl_file(path, tile_zoom, &mut seg_to_to_int, &mut seen_nodes,
                                  &mut edge_idx, &mut edge_app, &mut node_app, &pb)?;
            bar.inc(1);
        }
        bar.finish_and_clear();
    } else {
        process_geojsonl_file(roads_path, tile_zoom, &mut seg_to_to_int, &mut seen_nodes,
                              &mut edge_idx, &mut edge_app, &mut node_app, &pb)?;
    }

    edge_app.flush().context("flush q_edges")?;
    node_app.flush().context("flush q_nodes")?;
    drop(edge_app);
    drop(node_app);
    pb.finish_and_clear();

    let edge_count: i64 = conn.query_row("SELECT COUNT(*) FROM q_edges", [], |r| r.get(0))?;
    let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM q_nodes", [], |r| r.get(0))?;
    info!(edges = edge_count, nodes = node_count, "extract+quantize complete");

    Ok(seg_to_to_int)
}

// ── Phase 2: Load restrictions CSV ───────────────────────────────────────────

/// Load turn restrictions from an optional CSV into `restriction_triples`.
///
/// CSV columns:
///   2-column: from_segment_id, to_segment_id
///   3-column: from_segment_id, via_node_id, to_segment_id
///
/// For the 2-column form, via_node is derived from `seg_to_to_int[from_id]`.
fn load_restrictions(
    csv_path: &Path,
    conn: &Connection,
    seg_to_to_int: &std::collections::HashMap<i64, i64>,
) -> Result<usize> {
    let file = File::open(csv_path)
        .with_context(|| format!("open restrictions CSV {}", csv_path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0usize;

    conn.execute_batch("BEGIN").context("BEGIN restrictions")?;
    let result: Result<()> = (|| {
        let mut stmt = conn.prepare(
            "INSERT INTO restriction_triples VALUES (?, ?, ?, 0)",
        ).context("prepare INSERT restriction_triples")?;

        for (line_no, line_result) in reader.lines().enumerate() {
            let line = line_result
                .with_context(|| format!("read restrictions line {}", line_no + 1))?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }

            let cols: Vec<&str> = line.split(',').collect();
            let (from_id, via_node_id, to_id) = match cols.len() {
                2 => {
                    let from_id: i64 = cols[0].trim().parse()
                        .with_context(|| format!("bad from_id on line {}", line_no + 1))?;
                    let to_id: i64 = cols[1].trim().parse()
                        .with_context(|| format!("bad to_id on line {}", line_no + 1))?;
                    let via_node_id = match seg_to_to_int.get(&from_id) {
                        Some(&n) => n,
                        None => {
                            warn!(line = line_no + 1, from_id, "via_node not found, restriction skipped");
                            continue;
                        }
                    };
                    (from_id, via_node_id, to_id)
                }
                3 => {
                    let from_id: i64 = cols[0].trim().parse()
                        .with_context(|| format!("bad from_id on line {}", line_no + 1))?;
                    let via_node_id: i64 = cols[1].trim().parse()
                        .with_context(|| format!("bad via_node_id on line {}", line_no + 1))?;
                    let to_id: i64 = cols[2].trim().parse()
                        .with_context(|| format!("bad to_id on line {}", line_no + 1))?;
                    (from_id, via_node_id, to_id)
                }
                _ => {
                    warn!(line = line_no + 1, "unexpected column count, skipped");
                    continue;
                }
            };

            // Encode binary IDs: from/to are segment IDs, via is a node ID.
            let from_id = segment_id_bytes(from_id);
            let via_id  = node_id_bytes(via_node_id);
            let to_id   = segment_id_bytes(to_id);

            stmt.execute(params![&from_id[..], &via_id[..], &to_id[..]])
                .context("INSERT restriction")?;
            count += 1;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
        return result.map(|_| 0);
    }
    conn.execute_batch("COMMIT").context("COMMIT restrictions")?;
    Ok(count)
}

// ── Public entry point ────────────────────────────────────────────────────────

pub(crate) fn run_pipeline(
    roads_path:        &Path,
    restrictions_path: Option<&Path>,
    output_dir:        &Path,
    extent_slug:       &str,
    tile_zoom:         u8,
    duckdb_memory_mb:  Option<u64>,
    duckdb_temp_dir:   Option<&Path>,
    show_progress:     bool,
    compress:          bool,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;
    let default_tmp = output_dir.join(format!(".duckdb_tmp_{}", std::process::id()));
    let temp_dir    = duckdb_temp_dir.unwrap_or(&default_tmp);
    let _tmp_guard = duckdb_temp_dir.is_none().then(|| crate::build::TempDirGuard(default_tmp.clone()));

    let conn = setup_duckdb(duckdb_memory_mb, temp_dir)?;

    // Phase 1: extract + quantize.
    let seg_to_to_int = extract_quantize(roads_path, &conn, tile_zoom, show_progress)?;

    // Phase 2: restrictions (optional).
    if let Some(csv_path) = restrictions_path {
        let count = load_restrictions(csv_path, &conn, &seg_to_to_int)?;
        info!(count, "restrictions loaded");
    }

    // Phase 3: tile + write PMTiles.
    tile_from_duckdb(&conn, tile_zoom, output_dir, extent_slug, "generic", show_progress, compress)?;

    drop(conn);
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for a real bug: q_edges was created without a split_idx
    /// column even though tile_from_duckdb (shared with osm_low_memory) selects
    /// one unconditionally, so this whole path crashed with a DuckDB Binder
    /// Error at the tiling stage for every --roads --low-memory build.
    #[test]
    fn run_pipeline_end_to_end_does_not_crash_on_missing_split_idx() {
        let dir = tempfile::tempdir().unwrap();
        let roads_path = dir.path().join("roads.geojsonl");
        std::fs::write(
            &roads_path,
            concat!(
                r#"{"id":1,"frc":3,"fow":3,"flowdir":1,"from_int":100,"to_int":101,"geometry":{"type":"LineString","coordinates":[[4.90,51.40],[4.91,51.41]]}}"#, "\n",
                r#"{"id":2,"frc":4,"fow":3,"flowdir":1,"from_int":101,"to_int":102,"geometry":{"type":"LineString","coordinates":[[4.91,51.41],[4.92,51.42]]}}"#, "\n",
            ),
        )
        .unwrap();

        let output_dir = dir.path().join("out");
        run_pipeline(
            &roads_path,
            None,
            &output_dir,
            "test",
            12,
            None,
            None,
            false,
            false,
        )
        .expect("low-memory generic pipeline must not crash on valid input");

        let archive = output_dir.join("openlrlens-test-generic.pmtiles");
        let meta = std::fs::metadata(&archive).expect("PMTiles archive must be written");
        assert!(meta.len() > 127, "archive must contain at least a header and tile data");

        let bytes = std::fs::read(&archive).unwrap();
        assert_eq!(&bytes[0..7], b"PMTiles", "output must be a valid PMTiles archive");

        assert!(output_dir.join("manifest.json").exists(), "manifest.json must be written");
    }

    // ── ID encoding ───────────────────────────────────────────────────────────

    #[test]
    fn segment_and_node_id_bytes_are_disjoint() {
        // segment_id_bytes puts the integer in bytes 0..8; node_id_bytes in 8..16 —
        // same convention as osm_adapt::encode_way_id/encode_node_id, and for the
        // same reason: a segment key and a node key with the same numeric value must
        // never collide.
        let same_numeric = 42i64;
        assert_ne!(segment_id_bytes(same_numeric), node_id_bytes(same_numeric));
        assert_eq!(&segment_id_bytes(same_numeric)[8..16], &[0u8; 8]);
        assert_eq!(&node_id_bytes(same_numeric)[0..8], &[0u8; 8]);
    }

    #[test]
    fn map_flowdir_matches_documented_convention() {
        assert_eq!(map_flowdir(1), 1); // both
        assert_eq!(map_flowdir(2), 2); // backward
        assert_eq!(map_flowdir(3), 3); // forward
        assert_eq!(map_flowdir(0), 1); // unknown -> both
        assert_eq!(map_flowdir(99), 1); // unknown -> both
    }

    // ── GeoJSONL line parsing ─────────────────────────────────────────────────

    #[test]
    fn parse_line_accepts_flat_format() {
        let mut seg_to_to_int = std::collections::HashMap::new();
        let line = r#"{"id":1,"frc":3,"fow":3,"flowdir":1,"from_int":100,"to_int":101,"geometry":{"type":"LineString","coordinates":[[4.90,51.40],[4.91,51.41]]}}"#;
        let edge = parse_line(line, &mut seg_to_to_int).unwrap().expect("valid line must parse");
        assert_eq!(edge.frc, 3);
        assert_eq!(edge.fow, 3);
        assert_eq!(edge.direction, 1);
        assert_eq!(edge.start_id, node_id_bytes(100));
        assert_eq!(edge.end_id, node_id_bytes(101));
        assert_eq!(edge.parent_id, segment_id_bytes(1));
        assert_eq!(seg_to_to_int.get(&1), Some(&101));
    }

    #[test]
    fn parse_line_accepts_geojson_feature_format() {
        let mut seg_to_to_int = std::collections::HashMap::new();
        let line = r#"{"type":"Feature","properties":{"id":2,"frc":4,"fow":2,"flowdir":2,"from_int":5,"to_int":6},"geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}}"#;
        let edge = parse_line(line, &mut seg_to_to_int).unwrap().expect("valid Feature must parse");
        assert_eq!(edge.frc, 4);
        assert_eq!(edge.direction, 2); // backward
    }

    #[test]
    fn parse_line_clamps_out_of_range_frc_fow() {
        let mut seg_to_to_int = std::collections::HashMap::new();
        let line = r#"{"id":1,"frc":99,"fow":-5,"flowdir":1,"from_int":1,"to_int":2,"geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}}"#;
        let edge = parse_line(line, &mut seg_to_to_int).unwrap().unwrap();
        assert_eq!(edge.frc, 7, "frc must clamp to the max valid value, not silently overflow u8");
        assert_eq!(edge.fow, 0, "fow must clamp to the min valid value");
    }

    #[test]
    fn parse_line_rejects_degenerate_geometry() {
        let mut seg_to_to_int = std::collections::HashMap::new();
        let line = r#"{"id":1,"frc":3,"fow":3,"flowdir":1,"from_int":1,"to_int":2,"geometry":{"type":"LineString","coordinates":[[0,0]]}}"#;
        assert!(parse_line(line, &mut seg_to_to_int).unwrap().is_none(), "a single-point geometry must be skipped, not error");
    }

    #[test]
    fn parse_line_errors_on_missing_required_field() {
        let mut seg_to_to_int = std::collections::HashMap::new();
        let line = r#"{"id":1,"frc":3,"fow":3,"flowdir":1,"from_int":1,"geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}}"#;
        assert!(parse_line(line, &mut seg_to_to_int).is_err(), "a missing required field (to_int) must be a hard error, not silently defaulted");
    }

    #[test]
    fn parse_line_errors_on_invalid_json() {
        let mut seg_to_to_int = std::collections::HashMap::new();
        assert!(parse_line("not json at all", &mut seg_to_to_int).is_err());
    }
}
