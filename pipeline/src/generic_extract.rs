use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use openlr_graph::Direction;
use serde_json::Value;
use tracing::{info, trace, warn};

use crate::restrictions::RestrictionTriple;
use crate::split::{haversine_m, NodeRecord, SplitEdge};

// ── ID encoding ───────────────────────────────────────────────────────────────

/// Segment integer ID → 16-byte GERS-compatible ID.
/// Layout: (id as u64 LE)[0..8] || [0u8; 8]
/// Disjoint from node IDs (which have zero first-half).
fn segment_gers(id: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&(id as u64).to_le_bytes());
    b
}

/// Node integer ID → 16-byte GERS-compatible ID.
/// Layout: [0u8; 8] || (id as u64 LE)[0..8]
/// Disjoint from segment IDs (which have zero second-half).
fn node_gers(id: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&(id as u64).to_le_bytes());
    b
}

// ── Attribute mapping ─────────────────────────────────────────────────────────

fn map_flowdir(flowdir: i64) -> Direction {
    match flowdir {
        2 => Direction::Backward,
        3 => Direction::Forward,
        _ => Direction::Both, // 1 = bidirectional; anything unknown → Both
    }
}

fn polyline_length_m(pts: &[(f64, f64)]) -> f64 {
    pts.windows(2)
        .map(|w| haversine_m(w[0].0, w[0].1, w[1].0, w[1].1))
        .sum()
}

// ── Feature parsing ───────────────────────────────────────────────────────────

/// Parse a single GeoJSONL line.
/// Returns (SplitEdge, start NodeRecord, end NodeRecord) or None for degenerate geometry.
/// Also records segment_id → to_int for later restriction CSV resolution.
///
/// Accepts two formats:
///   Flat:    {"id":1,"frc":3,...,"geometry":{"type":"LineString","coordinates":[...]}}
///   Feature: {"type":"Feature","properties":{"id":1,"frc":3,...},"geometry":{...}}
fn parse_feature(
    line: &str,
    seg_to_to_int: &mut HashMap<i64, i64>,
) -> Result<Option<(SplitEdge, NodeRecord, NodeRecord)>> {
    let v: Value = serde_json::from_str(line).context("JSON parse")?;

    // Detect format: GeoJSON Feature has a nested "properties" object;
    // the flat format carries properties at the top level alongside "geometry".
    let (props, geom) = if let Some(p) = v.get("properties") {
        let g = v.get("geometry").context("missing geometry")?;
        (p, g)
    } else {
        // Flat format — the root object is both properties and geometry container.
        let g = v.get("geometry").context("missing geometry")?;
        (&v, g)
    };

    let id       = props.get("id")      .and_then(Value::as_i64).context("missing id")?;
    let frc_raw  = props.get("frc")     .and_then(Value::as_i64).context("missing frc")?;
    let fow_raw  = props.get("fow")     .and_then(Value::as_i64).context("missing fow")?;
    let flowdir  = props.get("flowdir") .and_then(Value::as_i64).context("missing flowdir")?;
    let from_int = props.get("from_int").and_then(Value::as_i64).context("missing from_int")?;
    let to_int   = props.get("to_int")  .and_then(Value::as_i64).context("missing to_int")?;

    let coords = geom
        .get("coordinates")
        .and_then(Value::as_array)
        .context("missing coordinates")?;

    if coords.len() < 2 {
        return Ok(None);
    }

    let geometry: Vec<(f64, f64)> = coords
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let arr = c.as_array().with_context(|| format!("coordinate[{i}] not array"))?;
            let lon = arr.first().and_then(Value::as_f64)
                .with_context(|| format!("coordinate[{i}] missing lon"))?;
            let lat = arr.get(1).and_then(Value::as_f64)
                .with_context(|| format!("coordinate[{i}] missing lat"))?;
            Ok((lon, lat))
        })
        .collect::<Result<Vec<_>>>()?;

    // FRC clamped to [0, 7] (Invariant: values > 7 treated as 7)
    let frc = frc_raw.clamp(0, 7) as u8;
    let fow = fow_raw.clamp(0, 7) as u8;
    let direction = map_flowdir(flowdir);
    let length_m  = polyline_length_m(&geometry);

    seg_to_to_int.insert(id, to_int);

    let start_gers = node_gers(from_int);
    let end_gers   = node_gers(to_int);
    let parent     = segment_gers(id);

    let edge = SplitEdge {
        start_node_gers: start_gers,
        end_node_gers:   end_gers,
        geometry:        geometry.clone(),
        length_m,
        frc,
        fow,
        direction,
        parent_gers_id:  parent,
    };
    let start_node = NodeRecord {
        gers_id: start_gers,
        lon: geometry[0].0,
        lat: geometry[0].1,
    };
    let end_node = NodeRecord {
        gers_id: end_gers,
        lon: geometry.last().unwrap().0,
        lat: geometry.last().unwrap().1,
    };

    Ok(Some((edge, start_node, end_node)))
}

// ── File / directory reading ──────────────────────────────────────────────────

fn read_geojsonl_file(
    path: &Path,
    edges: &mut Vec<SplitEdge>,
    nodes: &mut Vec<NodeRecord>,
    seg_to_to_int: &mut HashMap<i64, i64>,
) -> Result<()> {
    let file = File::open(path)
        .with_context(|| format!("open {}", path.display()))?;

    let path_str = path.to_string_lossy().to_lowercase();
    let reader: Box<dyn BufRead> = if path_str.ends_with(".gz") {
        Box::new(BufReader::new(GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };

    let mut n_ok = 0usize;
    let mut n_skip = 0usize;

    for (line_no, line_result) in reader.lines().enumerate() {
        let line = line_result
            .with_context(|| format!("read line {} of {}", line_no + 1, path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match parse_feature(line, seg_to_to_int) {
            Ok(Some((edge, sn, en))) => {
                edges.push(edge);
                nodes.push(sn);
                nodes.push(en);
                n_ok += 1;
            }
            Ok(None) => {
                trace!(line = line_no + 1, "degenerate geometry, skipped");
                n_skip += 1;
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    line = line_no + 1,
                    error = %e,
                    "parse error, skipped"
                );
                n_skip += 1;
            }
        }
    }

    info!(
        path     = %path.display(),
        segments = n_ok,
        skipped  = n_skip,
        "loaded"
    );
    Ok(())
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Extract SplitEdges and NodeRecords from a GeoJSONL(.gz) file or directory.
///
/// `roads_path` may be:
///   - A single `.geojsonl`, `.geojsonl.gz`, or `.geojson.gz` file.
///   - A directory; all matching files inside are processed in sorted order.
///
/// Returns (edges, nodes, segment_id_to_to_int).
/// The third element is needed to resolve via-node IDs in the restriction CSV
/// when the via column is omitted.
pub fn extract(
    roads_path: &Path,
) -> Result<(Vec<SplitEdge>, Vec<NodeRecord>, HashMap<i64, i64>)> {
    let mut edges = Vec::new();
    let mut nodes = Vec::new();
    let mut seg_to_to_int: HashMap<i64, i64> = HashMap::new();

    if roads_path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(roads_path)
            .with_context(|| format!("read dir {}", roads_path.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.to_string_lossy().to_lowercase();
                name.ends_with(".geojsonl")
                    || name.ends_with(".geojsonl.gz")
                    || name.ends_with(".geojson.gz")
            })
            .collect();
        entries.sort();

        anyhow::ensure!(
            !entries.is_empty(),
            "no .geojsonl or .geojsonl.gz files found in {}",
            roads_path.display()
        );

        for path in &entries {
            read_geojsonl_file(path, &mut edges, &mut nodes, &mut seg_to_to_int)?;
        }
    } else {
        read_geojsonl_file(roads_path, &mut edges, &mut nodes, &mut seg_to_to_int)?;
    }

    info!(
        total_edges = edges.len(),
        total_nodes = nodes.len(),
        "generic extract complete"
    );
    Ok((edges, nodes, seg_to_to_int))
}

/// Load turn restrictions from a CSV file.
///
/// Expected columns (header row optional):
///   from_segment_id, [via_node_id,] to_segment_id
///
/// If `via_node_id` is omitted (2-column form), it is derived as the `to_int`
/// of the from-segment, using the `seg_to_to_int` map populated during extract.
pub fn read_restrictions_csv(
    csv_path: &Path,
    seg_to_to_int: &HashMap<i64, i64>,
) -> Result<Vec<RestrictionTriple>> {
    let content = std::fs::read_to_string(csv_path)
        .with_context(|| format!("read restrictions CSV {}", csv_path.display()))?;

    let mut out = Vec::new();
    let mut n_skip = 0usize;
    let mut first_data_line = true;

    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Skip header row: first line that starts with a non-digit, non-minus character.
        if first_data_line {
            first_data_line = false;
            if !line.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
                continue;
            }
        }

        let parts: Vec<&str> = line.split(',').collect();

        let (from_id, via_node_id, to_id) = if parts.len() >= 3 {
            let from: i64 = match parts[0].trim().parse() {
                Ok(v) => v,
                Err(_) => {
                    warn!(line = line_no + 1, "bad from_segment_id, skipped");
                    n_skip += 1;
                    continue;
                }
            };
            let via: i64 = match parts[1].trim().parse() {
                Ok(v) => v,
                Err(_) => {
                    warn!(line = line_no + 1, "bad via_node_id, skipped");
                    n_skip += 1;
                    continue;
                }
            };
            let to: i64 = match parts[2].trim().parse() {
                Ok(v) => v,
                Err(_) => {
                    warn!(line = line_no + 1, "bad to_segment_id, skipped");
                    n_skip += 1;
                    continue;
                }
            };
            (from, via, to)
        } else if parts.len() == 2 {
            let from: i64 = match parts[0].trim().parse() {
                Ok(v) => v,
                Err(_) => {
                    warn!(line = line_no + 1, "bad from_segment_id, skipped");
                    n_skip += 1;
                    continue;
                }
            };
            let to: i64 = match parts[1].trim().parse() {
                Ok(v) => v,
                Err(_) => {
                    warn!(line = line_no + 1, "bad to_segment_id, skipped");
                    n_skip += 1;
                    continue;
                }
            };
            let via = match seg_to_to_int.get(&from) {
                Some(&v) => v,
                None => {
                    warn!(
                        line = line_no + 1,
                        from_id = from,
                        "from_segment_id not in roads data, cannot derive via_node, skipped"
                    );
                    n_skip += 1;
                    continue;
                }
            };
            (from, via, to)
        } else {
            warn!(line = line_no + 1, "expected 2 or 3 columns, skipped");
            n_skip += 1;
            continue;
        };

        out.push(RestrictionTriple {
            from_segment_gers:  segment_gers(from_id),
            via_connector_gers: node_gers(via_node_id),
            to_segment_gers:    segment_gers(to_id),
            flags:              0,
        });
    }

    info!(
        path         = %csv_path.display(),
        restrictions = out.len(),
        skipped      = n_skip,
        "restrictions CSV loaded"
    );
    Ok(out)
}
