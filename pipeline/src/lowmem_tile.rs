//! Shared post-extraction stage of every DuckDB-backed ("low-memory") ingestion
//! path: tile-payload building and PMTiles writing from the common `q_edges` /
//! `q_nodes` / `restriction_triples` scratch tables.
//!
//! Every low-memory producer (`osm_low_memory`, `generic_low_memory`, and the
//! canonical-DuckDB-input path) populates those three tables however suits its
//! source format, then calls `tile_from_duckdb` to do the rest. This keeps the
//! tile-writing logic — Hilbert-order streaming, boundary-node detection,
//! restriction resolution, string-pool encoding — implemented exactly once.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use duckdb::Connection;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{info, warn};

use crate::merge::StreamingWriter;

// ── Internal structs ──────────────────────────────────────────────────────────

/// Edge data fetched from DuckDB for tile payload building.
///
/// `stable_id` is the exact string to persist in the tile's string pool —
/// producers compute it once at extraction time (see `q_edges.stable_id`) and
/// it's carried straight through, rather than being reconstructed here from
/// the internal dedup key. That reconstruction (`tile::seg_stable_id_str`)
/// only round-trips two shapes (a plain integer, or exactly 16 raw bytes) —
/// fine for OSM/Overture-style ids, but not for an arbitrary producer string
/// up to 255 bytes (see `canonical_schema.sql`). Carrying the real string
/// explicitly makes every producer's id handling equally general.
struct LmEdge {
    edge_idx: u32,
    stable_id: String,
    start_id: [u8; 16],
    end_id: [u8; 16],
    geom: Vec<(i32, i32)>,
    length_cm: u32,
    frc: u8,
    fow: u8,
    direction: u8,
}

struct LmIntraTile {
    from_seg: u32,
    via_node: u32,
    to_seg: u32,
    flags: u8,
}

struct LmCrossTile {
    from_id: String,
    via_node_local: u32,
    to_id: String,
    flags: u8,
}

struct ResolvedRestriction {
    via_id: [u8; 16],
    flags: u8,
    from_edge_idx: u32,
    to_edge_idx: u32,
    from_stable_id: String,
    to_stable_id: String,
    via_tile_x: u32,
    via_tile_y: u32,
}

// ── BLOB ↔ Rust helpers ───────────────────────────────────────────────────────

pub(crate) fn geom_to_blob(geom: &[(i32, i32)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(geom.len() * 8);
    for (x, y) in geom {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
    }
    b
}

fn blob_to_geom(blob: &[u8]) -> Vec<(i32, i32)> {
    blob.chunks_exact(8)
        .map(|c| {
            let x = i32::from_le_bytes(c[0..4].try_into().unwrap());
            let y = i32::from_le_bytes(c[4..8].try_into().unwrap());
            (x, y)
        })
        .collect()
}

fn blob_to_id(blob: &[u8]) -> [u8; 16] {
    blob.try_into().expect("ID blob must be 16 bytes")
}

// ── Tile payload building (mirrors tile.rs; kept here to avoid cross-module coupling) ─

fn pack_attrs_lm(frc: u8, fow: u8, direction: u8) -> u8 {
    (frc & 0x07) | ((fow & 0x07) << 3) | ((direction & 0x03) << 6)
}

fn compute_tile_nodes_lm(edges: &[LmEdge]) -> (Vec<[u8; 16]>, HashMap<[u8; 16], u32>) {
    let mut order: Vec<[u8; 16]> = Vec::new();
    let mut index: HashMap<[u8; 16], u32> = HashMap::new();
    for e in edges {
        for &nid in &[e.start_id, e.end_id] {
            if !index.contains_key(&nid) {
                let i = order.len() as u32;
                order.push(nid);
                index.insert(nid, i);
            }
        }
    }
    (order, index)
}

fn build_lm_tile_payload(
    edges: &[LmEdge],
    node_order: &[[u8; 16]],
    node_index: &HashMap<[u8; 16], u32>,
    node_lookup: &HashMap<[u8; 16], (i32, i32)>,
    node_to_tile: &HashMap<[u8; 16], (u32, u32)>,
    node_stable_id: &HashMap<[u8; 16], String>,
    tile_x: u32,
    tile_y: u32,
    intra: &[LmIntraTile],
    cross: &[LmCrossTile],
) -> Vec<u8> {
    let segment_count      = edges.len() as u32;
    let node_count         = node_order.len() as u32;
    let restriction_count  = intra.len() as u32;
    let xrestriction_count = cross.len() as u32;

    // ── Build string pool ────────────────────────────────────────────────────────
    let mut string_pool: Vec<u8> = Vec::new();

    let mut seg_id_pool: Vec<(u32, u8)> = Vec::with_capacity(edges.len());
    for e in edges.iter() {
        let off = string_pool.len() as u32;
        let len = e.stable_id.len().min(255) as u8;
        string_pool.extend_from_slice(&e.stable_id.as_bytes()[..len as usize]);
        seg_id_pool.push((off, len));
    }

    let mut node_id_pool: Vec<(u32, u8)> = Vec::with_capacity(node_order.len());
    for node_id in node_order.iter() {
        let empty = String::new();
        let s = node_stable_id.get(node_id).unwrap_or_else(|| {
            warn!(id = %hex::encode(node_id), "node stable_id not found");
            &empty
        });
        let off = string_pool.len() as u32;
        let len = s.len().min(255) as u8;
        string_pool.extend_from_slice(&s.as_bytes()[..len as usize]);
        node_id_pool.push((off, len));
    }

    let mut cross_id_pool: Vec<(u32, u8, u32, u8)> = Vec::with_capacity(cross.len());
    for r in cross.iter() {
        let from_off = string_pool.len() as u32;
        let from_len = r.from_id.len().min(255) as u8;
        string_pool.extend_from_slice(&r.from_id.as_bytes()[..from_len as usize]);
        let to_off = string_pool.len() as u32;
        let to_len = r.to_id.len().min(255) as u8;
        string_pool.extend_from_slice(&r.to_id.as_bytes()[..to_len as usize]);
        cross_id_pool.push((from_off, from_len, to_off, to_len));
    }

    let string_pool_length = string_pool.len() as u32;

    // ── Geometry pool and segment records ────────────────────────────────────────
    let mut geom_pool:   Vec<(i32, i32)> = Vec::new();
    let mut seg_records: Vec<[u8; 32]>   = Vec::with_capacity(edges.len());

    for (local_idx, e) in edges.iter().enumerate() {
        let geom_offset = geom_pool.len() as u32;
        let geom_len    = e.geom.len() as u16;
        geom_pool.extend_from_slice(&e.geom);

        let start_node = node_index[&e.start_id];
        let end_node   = node_index[&e.end_id];
        let packed     = pack_attrs_lm(e.frc, e.fow, e.direction);
        let (sid_off, sid_len) = seg_id_pool[local_idx];

        let mut r = [0u8; 32];
        r[0..4].copy_from_slice(&start_node.to_le_bytes());
        r[4..8].copy_from_slice(&end_node.to_le_bytes());
        r[8..12].copy_from_slice(&geom_offset.to_le_bytes());
        r[12..14].copy_from_slice(&geom_len.to_le_bytes());
        r[14..18].copy_from_slice(&e.length_cm.to_le_bytes());
        r[18] = packed;
        r[20..24].copy_from_slice(&sid_off.to_le_bytes());
        r[24] = sid_len;
        seg_records.push(r);
    }

    let geom_vertex_count = geom_pool.len() as u32;

    // ── Header (v3) ───────────────────────────────────────────────────────────────
    let mut hdr = [0u8; 40];
    hdr[0..4].copy_from_slice(b"OLRL");
    hdr[4] = 3;
    hdr[8..12].copy_from_slice(&segment_count.to_le_bytes());
    hdr[12..16].copy_from_slice(&node_count.to_le_bytes());
    hdr[16..20].copy_from_slice(&restriction_count.to_le_bytes());
    hdr[20..24].copy_from_slice(&geom_vertex_count.to_le_bytes());
    hdr[24..28].copy_from_slice(&xrestriction_count.to_le_bytes());
    hdr[28..32].copy_from_slice(&string_pool_length.to_le_bytes());

    let cap = 40
        + seg_records.len() * 32
        + geom_pool.len() * 8
        + node_order.len() * 28
        + intra.len() * 16
        + cross.len() * 16
        + string_pool.len();
    let mut payload = Vec::with_capacity(cap);

    payload.extend_from_slice(&hdr);
    for r in &seg_records { payload.extend_from_slice(r); }
    for (lon_e7, lat_e7) in &geom_pool {
        payload.extend_from_slice(&lon_e7.to_le_bytes());
        payload.extend_from_slice(&lat_e7.to_le_bytes());
    }
    for (node_local_idx, node_id) in node_order.iter().enumerate() {
        let (lon_e7, lat_e7) = node_lookup.get(node_id).copied().unwrap_or_else(|| {
            warn!(id = %hex::encode(node_id), "node not found in lookup");
            (0, 0)
        });
        let is_boundary = node_to_tile.get(node_id) != Some(&(tile_x, tile_y));
        let (nid_off, nid_len) = node_id_pool[node_local_idx];
        payload.extend_from_slice(&lon_e7.to_le_bytes());
        payload.extend_from_slice(&lat_e7.to_le_bytes());
        payload.extend_from_slice(&nid_off.to_le_bytes());
        payload.push(nid_len);
        payload.extend_from_slice(&[0u8; 11]);
        payload.push(u8::from(is_boundary));
        payload.extend_from_slice(&[0u8; 3]);
    }
    for r in intra {
        payload.extend_from_slice(&r.from_seg.to_le_bytes());
        payload.extend_from_slice(&r.via_node.to_le_bytes());
        payload.extend_from_slice(&r.to_seg.to_le_bytes());
        payload.push(r.flags);
        payload.extend_from_slice(&[0u8; 3]);
    }
    for (r, (from_off, from_len, to_off, to_len)) in cross.iter().zip(cross_id_pool.iter()) {
        payload.extend_from_slice(&from_off.to_le_bytes());
        payload.push(*from_len);
        payload.extend_from_slice(&r.via_node_local.to_le_bytes());
        payload.extend_from_slice(&to_off.to_le_bytes());
        payload.push(*to_len);
        payload.push(r.flags);
        payload.push(0u8);
    }
    payload.extend_from_slice(&string_pool);
    payload
}

// ── Progress bar helpers ──────────────────────────────────────────────────────

pub(crate) fn make_spinner(show: bool, msg: &'static str) -> ProgressBar {
    if !show { return ProgressBar::hidden(); }
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg} [{elapsed_precise}]")
            .expect("valid template"),
    );
    pb.set_message(msg);
    pb.enable_steady_tick(Duration::from_millis(120));
    pb
}

/// Progress bar that displays `bytes_read / total_bytes` with a percentage bar.
/// Used for the two PBF scan passes where file size is the natural denominator.
pub(crate) fn make_bytes_bar(show: bool, total_bytes: u64, msg: &'static str) -> ProgressBar {
    if !show { return ProgressBar::hidden(); }
    let pb = ProgressBar::new(total_bytes);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg:32} [{bar:40.cyan/blue}] {bytes}/{total_bytes}  eta {eta}")
            .expect("valid template")
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_message(msg);
    pb
}

pub(crate) fn make_bar(show: bool, total: u64, msg: &'static str) -> ProgressBar {
    if !show { return ProgressBar::hidden(); }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg:32} [{bar:40.cyan/blue}] {human_pos}/{human_len}  eta {eta}")
            .expect("valid template")
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_message(msg);
    pb
}

/// Lossless collinear-vertex removal (mirror of quantize::remove_collinear).
pub(crate) fn remove_collinear_lm(pts: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    if pts.len() <= 2 { return pts; }
    let mut out = Vec::with_capacity(pts.len());
    out.push(pts[0]);
    for i in 1..pts.len() - 1 {
        let (x0, y0) = out.last().copied().unwrap();
        let (x1, y1) = pts[i];
        let (x2, y2) = pts[i + 1];
        let cross = (x1 - x0) as i64 * (y2 - y0) as i64
                  - (y1 - y0) as i64 * (x2 - x0) as i64;
        if cross != 0 { out.push(pts[i]); }
    }
    out.push(*pts.last().unwrap());
    out
}

// ── Tile from DuckDB → PMTiles ────────────────────────────────────────────────

/// Read the common `q_edges` / `q_nodes` / `restriction_triples` scratch tables
/// and write a PMTiles archive, streaming one tile at a time in Hilbert order.
/// Shared by every low-memory ingestion path regardless of source format.
pub(crate) fn tile_from_duckdb(
    conn: &Connection,
    tile_zoom: u8,
    output_dir: &Path,
    extent_slug: &str,
    release_label: &str,
    show_progress: bool,
    compress: bool,
) -> Result<()> {
    // ── Load all nodes into RAM ───────────────────────────────────────────────
    // For Europe this is ~20 M nodes × (16+8+4+4+~20) bytes ≈ 1 GB.  Acceptable.
    let mut node_lookup:    HashMap<[u8; 16], (i32, i32)> = HashMap::new();
    let mut node_to_tile:   HashMap<[u8; 16], (u32, u32)> = HashMap::new();
    let mut node_stable_id: HashMap<[u8; 16], String>     = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT node_id, lon_e7, lat_e7, tile_x, tile_y, stable_id FROM q_nodes"
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let blob: Vec<u8> = row.get(0)?;
            let nid = blob_to_id(&blob);
            let lon_e7: i32 = row.get(1)?;
            let lat_e7: i32 = row.get(2)?;
            let tile_x: u32 = row.get::<_, i64>(3)? as u32;
            let tile_y: u32 = row.get::<_, i64>(4)? as u32;
            let stable_id: String = row.get(5)?;
            node_lookup.insert(nid, (lon_e7, lat_e7));
            node_to_tile.insert(nid, (tile_x, tile_y));
            node_stable_id.insert(nid, stable_id);
        }
    }
    info!(nodes = node_lookup.len(), "node_lookup loaded");

    // ── Scan edge metadata to build edge maps and tile count ─────────────────────
    let mut seen_tile_ids: HashSet<u64>  = HashSet::new();
    let mut from_edge_map: HashMap<([u8; 16], [u8; 16]), u32> = HashMap::new();
    let mut to_edge_map:   HashMap<([u8; 16], [u8; 16]), u32> = HashMap::new();
    let mut edge_stable_id: HashMap<u32, String> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT edge_idx, start_id, end_id, parent_id, tile_id, stable_id FROM q_edges"
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let edge_idx:  u32 = row.get::<_, i64>(0)? as u32;
            let start_blob:  Vec<u8> = row.get(1)?;
            let end_blob:    Vec<u8> = row.get(2)?;
            let parent_blob: Vec<u8> = row.get(3)?;
            let tile_id: u64 = row.get::<_, i64>(4)? as u64;
            let stable_id: String = row.get(5)?;

            let start_id  = blob_to_id(&start_blob);
            let end_id    = blob_to_id(&end_blob);
            let parent_id = blob_to_id(&parent_blob);

            seen_tile_ids.insert(tile_id);

            // Same edge may appear twice (start tile + end tile); HashMap insert is
            // idempotent here since both rows carry identical (parent_id, end/start_id).
            from_edge_map.insert((parent_id, end_id),   edge_idx);
            to_edge_map.insert(  (parent_id, start_id), edge_idx);
            edge_stable_id.insert(edge_idx, stable_id);
        }
    }
    let total_tiles = seen_tile_ids.len();
    drop(seen_tile_ids);

    info!(tiles = total_tiles, "tile metadata scanned");

    // ── Resolve restrictions ──────────────────────────────────────────────────
    let mut resolved: Vec<ResolvedRestriction> = Vec::new();
    let mut n_skipped = 0usize;
    {
        let mut stmt = conn.prepare("SELECT from_id, via_id, to_id, flags FROM restriction_triples")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let from_blob: Vec<u8> = row.get(0)?;
            let via_blob:  Vec<u8> = row.get(1)?;
            let to_blob:   Vec<u8> = row.get(2)?;
            let flags: u8          = row.get::<_, i64>(3)? as u8;

            let from_id = blob_to_id(&from_blob);
            let via_id  = blob_to_id(&via_blob);
            let to_id   = blob_to_id(&to_blob);

            let from_edge_idx = match from_edge_map.get(&(from_id, via_id)) {
                Some(&i) => i, None => { n_skipped += 1; continue; }
            };
            let to_edge_idx = match to_edge_map.get(&(to_id, via_id)) {
                Some(&i) => i, None => { n_skipped += 1; continue; }
            };
            let (via_tile_x, via_tile_y) = match node_to_tile.get(&via_id) {
                Some(&t) => t, None => { n_skipped += 1; continue; }
            };
            let from_stable_id = edge_stable_id.get(&from_edge_idx).cloned().unwrap_or_default();
            let to_stable_id   = edge_stable_id.get(&to_edge_idx).cloned().unwrap_or_default();

            resolved.push(ResolvedRestriction {
                via_id, flags,
                from_edge_idx, to_edge_idx,
                from_stable_id, to_stable_id,
                via_tile_x, via_tile_y,
            });
        }
    }
    if !resolved.is_empty() || n_skipped > 0 {
        info!(resolved = resolved.len(), skipped = n_skipped, "restrictions resolved");
    }

    // Group resolved restrictions by via tile.
    let mut tile_restrictions: HashMap<(u32, u32), Vec<&ResolvedRestriction>> = HashMap::new();
    for r in &resolved {
        tile_restrictions
            .entry((r.via_tile_x, r.via_tile_y))
            .or_default()
            .push(r);
    }

    // ── Stream all edges ordered by Hilbert tile_id — one scan, zero per-tile queries ──
    // DuckDB sorts 70M rows by tile_id (in-memory with the available budget) once,
    // then streams them back.  We accumulate edges for the current tile and flush
    // each tile as soon as the tile_id changes, so peak RAM is one tile's edges.
    let safe_release = release_label.replace('.', "-");
    let archive_filename = format!("openlrlens-{extent_slug}-{safe_release}.pmtiles");
    let archive_path = output_dir.join(&archive_filename);

    let mut writer = StreamingWriter::new_in(output_dir, compress).context("create StreamingWriter")?;
    let pb = make_bar(show_progress, total_tiles as u64, "Tiling              ");
    let mut done_tiles = 0usize;

    {
        let mut stmt = conn.prepare(
            "SELECT edge_idx, start_id, end_id, geom_blob, length_cm, \
                    frc, fow, direction, tile_x, tile_y, tile_id, stable_id \
             FROM q_edges ORDER BY tile_id, edge_idx"
        ).context("prepare tiling scan")?;
        let mut rows = stmt.query([]).context("execute tiling scan")?;

        let mut cur_tile:  Option<(u32, u32, u64)> = None; // (tile_x, tile_y, tile_id)
        let mut cur_edges: Vec<LmEdge>              = Vec::new();

        while let Some(row) = rows.next()? {
            let edge_idx:  u32 = row.get::<_, i64>(0)? as u32;
            let start:  Vec<u8> = row.get(1)?;
            let end:    Vec<u8> = row.get(2)?;
            let geom:   Vec<u8> = row.get(3)?;
            let len_cm: u32     = row.get::<_, i64>(4)? as u32;
            let frc:    u8      = row.get::<_, i64>(5)? as u8;
            let fow:    u8      = row.get::<_, i64>(6)? as u8;
            let dir:    u8      = row.get::<_, i64>(7)? as u8;
            let tile_x: u32     = row.get::<_, i64>(8)? as u32;
            let tile_y: u32     = row.get::<_, i64>(9)? as u32;
            let tile_id: u64    = row.get::<_, i64>(10)? as u64;
            let stable_id: String = row.get(11)?;

            if let Some((cx, cy, cid)) = cur_tile {
                if (tile_x, tile_y) != (cx, cy) {
                    flush_tile(cid, cx, cy, &cur_edges,
                               &node_lookup, &node_to_tile, &node_stable_id, &tile_restrictions,
                               &mut writer)?;
                    cur_edges.clear();
                    done_tiles += 1;
                    pb.inc(1);
                }
            }
            cur_tile = Some((tile_x, tile_y, tile_id));
            cur_edges.push(LmEdge {
                edge_idx,
                stable_id,
                start_id:  blob_to_id(&start),
                end_id:    blob_to_id(&end),
                geom:        blob_to_geom(&geom),
                length_cm:   len_cm,
                frc, fow, direction: dir,
            });
        }

        // Flush the final tile.
        if let Some((cx, cy, cid)) = cur_tile {
            if !cur_edges.is_empty() {
                flush_tile(cid, cx, cy, &cur_edges,
                           &node_lookup, &node_to_tile, &node_stable_id, &tile_restrictions,
                           &mut writer)?;
                done_tiles += 1;
                pb.inc(1);
            }
        }
    }

    pb.finish_and_clear();
    writer.finish(&archive_path, tile_zoom).context("finish PMTiles")?;
    info!(path = %archive_path.display(), tiles = done_tiles, "PMTiles archive written");

    // Write manifest.json
    write_lm_manifest(output_dir, &archive_filename, release_label, extent_slug, tile_zoom)?;
    Ok(())
}

fn flush_tile(
    tile_id: u64,
    tile_x: u32,
    tile_y: u32,
    edges: &[LmEdge],
    node_lookup: &HashMap<[u8; 16], (i32, i32)>,
    node_to_tile: &HashMap<[u8; 16], (u32, u32)>,
    node_stable_id: &HashMap<[u8; 16], String>,
    tile_restrictions: &HashMap<(u32, u32), Vec<&ResolvedRestriction>>,
    writer: &mut StreamingWriter,
) -> Result<()> {
    let (node_order, node_index) = compute_tile_nodes_lm(edges);

    let mut intra: Vec<LmIntraTile> = Vec::new();
    let mut cross: Vec<LmCrossTile> = Vec::new();

    if let Some(restrs) = tile_restrictions.get(&(tile_x, tile_y)) {
        // Build local edge index once per tile (replaces the O(N) tile_bins scan).
        let local_for_edge: HashMap<u32, u32> = edges
            .iter()
            .enumerate()
            .map(|(i, e)| (e.edge_idx, i as u32))
            .collect();

        for r in restrs {
            let Some(&via_node_local) = node_index.get(&r.via_id) else { continue };

            // A restriction is intra-tile iff both its from and to edges are in this tile.
            let is_intra = local_for_edge.contains_key(&r.from_edge_idx)
                        && local_for_edge.contains_key(&r.to_edge_idx);

            if is_intra {
                if let (Some(&fl), Some(&tl)) = (
                    local_for_edge.get(&r.from_edge_idx),
                    local_for_edge.get(&r.to_edge_idx),
                ) {
                    intra.push(LmIntraTile { from_seg: fl, via_node: via_node_local, to_seg: tl, flags: r.flags });
                }
            } else {
                cross.push(LmCrossTile {
                    from_id: r.from_stable_id.clone(),
                    via_node_local,
                    to_id: r.to_stable_id.clone(),
                    flags: r.flags,
                });
            }
        }
    }

    let payload = build_lm_tile_payload(
        edges, &node_order, &node_index,
        node_lookup, node_to_tile, node_stable_id, tile_x, tile_y, &intra, &cross,
    );
    writer.add_tile(tile_id, &payload).context("add_tile")?;
    Ok(())
}

fn write_lm_manifest(
    output_dir: &Path,
    archive_filename: &str,
    release: &str,
    extent_slug: &str,
    tile_zoom: u8,
) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let built_at = format!("{}Z", secs); // epoch seconds, close enough for manifest

    let manifest = serde_json::json!({
        "archive":   archive_filename,
        "release":   release,
        "extent":    extent_slug,
        "tile_zoom": tile_zoom,
        "built_at":  built_at,
    });
    let path = output_dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write {}", path.display()))?;
    info!(path = %path.display(), "manifest written");
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::params;
    use crate::tile::{lon_lat_to_tile_xy, xyz_to_tile_id};

    // ── BLOB ↔ Rust helpers ───────────────────────────────────────────────────

    #[test]
    fn geom_blob_roundtrips() {
        let geom = vec![(1_747_700_000i32, -366_000_000), (1_748_000_000, -365_500_000), (0, 0)];
        let blob = geom_to_blob(&geom);
        assert_eq!(blob.len(), geom.len() * 8);
        assert_eq!(blob_to_geom(&blob), geom);
    }

    #[test]
    fn geom_blob_empty_roundtrips() {
        let geom: Vec<(i32, i32)> = vec![];
        assert_eq!(blob_to_geom(&geom_to_blob(&geom)), geom);
    }

    #[test]
    fn blob_to_id_reads_16_bytes() {
        let id = [7u8; 16];
        assert_eq!(blob_to_id(&id), id);
    }

    #[test]
    #[should_panic(expected = "16 bytes")]
    fn blob_to_id_rejects_wrong_length() {
        blob_to_id(&[1, 2, 3]);
    }

    // ── Attribute packing ─────────────────────────────────────────────────────

    #[test]
    fn pack_attrs_lm_matches_tile_rs_bit_layout() {
        // frc[2:0] | fow[5:3] | direction[7:6] — must match tile::pack_attrs exactly,
        // since both are read by the same decoder.
        let packed = pack_attrs_lm(3, 5, 2);
        assert_eq!(packed & 0x07, 3);
        assert_eq!((packed >> 3) & 0x07, 5);
        assert_eq!((packed >> 6) & 0x03, 2);
    }

    // ── Collinear removal (mirror of quantize::remove_collinear) ─────────────

    #[test]
    fn collinear_removal_keeps_endpoints() {
        let pts = vec![(0i32, 0), (0, 5), (0, 10)];
        assert_eq!(remove_collinear_lm(pts), vec![(0, 0), (0, 10)]);
    }

    #[test]
    fn collinear_removal_keeps_bend() {
        let pts = vec![(0i32, 0), (1, 5), (2, 0)];
        assert_eq!(remove_collinear_lm(pts).len(), 3);
    }

    #[test]
    fn collinear_removal_chain() {
        let pts = vec![(0i32, 0), (1, 1), (2, 2), (3, 1)];
        assert_eq!(remove_collinear_lm(pts), vec![(0, 0), (2, 2), (3, 1)]);
    }

    #[test]
    fn two_point_line_unchanged_lm() {
        let pts = vec![(0i32, 0), (1, 1)];
        assert_eq!(remove_collinear_lm(pts.clone()), pts);
    }

    // ── Tile-local node ordering ──────────────────────────────────────────────

    fn make_edge(edge_idx: u32, stable_id: &str, start: [u8; 16], end: [u8; 16]) -> LmEdge {
        LmEdge {
            edge_idx,
            stable_id: stable_id.to_string(),
            start_id: start,
            end_id: end,
            geom: vec![(0, 0), (1, 1)],
            length_cm: 100,
            frc: 3,
            fow: 3,
            direction: 0,
        }
    }

    #[test]
    fn compute_tile_nodes_dedupes_shared_endpoints() {
        // Two edges sharing node B: A-B, B-C — B must appear exactly once.
        let a = [1u8; 16];
        let b = [2u8; 16];
        let c = [3u8; 16];
        let edges = vec![make_edge(0, "e0", a, b), make_edge(1, "e1", b, c)];
        let (order, index) = compute_tile_nodes_lm(&edges);
        assert_eq!(order.len(), 3, "A, B, C must each appear exactly once");
        assert_eq!(order[index[&a] as usize], a);
        assert_eq!(order[index[&b] as usize], b);
        assert_eq!(order[index[&c] as usize], c);
    }

    // ── tile_from_duckdb: endpoint-duplication + per-tile is_boundary ─────────
    //
    // Directly exercises the exact invariant tile.rs::write_tiles was fixed to
    // match: an edge whose endpoints fall in different tiles must be present in
    // BOTH tiles, and a node's is_boundary flag is relative to whichever tile is
    // being decoded, not a single global flag.

    fn create_scratch_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE q_edges ( \
                 edge_idx INTEGER, split_idx INTEGER, \
                 start_id BLOB, end_id BLOB, parent_id BLOB, \
                 geom_blob BLOB, length_cm INTEGER, \
                 frc INTEGER, fow INTEGER, direction INTEGER, \
                 tile_x INTEGER, tile_y INTEGER, tile_id BIGINT, stable_id VARCHAR); \
             CREATE TABLE q_nodes (node_id BLOB, lon_e7 INTEGER, lat_e7 INTEGER, tile_x INTEGER, tile_y INTEGER, stable_id VARCHAR); \
             CREATE TABLE restriction_triples (from_id BLOB, via_id BLOB, to_id BLOB, flags INTEGER);"
        ).unwrap();
    }

    #[test]
    fn boundary_spanning_edge_is_duplicated_into_both_tiles() {
        // A realistic ~150 m edge at z12 (the production zoom), straddling exactly
        // one tile boundary in x: lon=-0.001 -> tile x=2047; lon=0.001 -> tile x=2048;
        // both at lat=45 (comfortably clear of a y boundary). Deliberately NOT using a
        // huge/antipodal span here — Graph::add_segment indexes a spatial grid cell for
        // every ~222m cell in an edge's bounding box, so an unrealistically long test
        // edge (e.g. spanning whole-earth coordinates) blows up to billions of cells.
        let tile_zoom = 12u8;
        let (a_tx, a_ty) = lon_lat_to_tile_xy(-0.001, 45.0, tile_zoom);
        let (b_tx, b_ty) = lon_lat_to_tile_xy(0.001, 45.0, tile_zoom);
        assert_eq!((a_tx, a_ty), (2047, 1473));
        assert_eq!((b_tx, b_ty), (2048, 1473));

        let node_a: [u8; 16] = [1u8; 16];
        let node_b: [u8; 16] = [2u8; 16];
        let edge_ab: [u8; 16] = [9u8; 16];

        let conn = Connection::open_in_memory().unwrap();
        create_scratch_schema(&conn);

        conn.execute(
            "INSERT INTO q_nodes VALUES (?, ?, ?, ?, ?, ?)",
            params![&node_a[..], -10_000i32, 450_000_000i32, a_tx as i64, a_ty as i64, "nodeA"],
        ).unwrap();
        conn.execute(
            "INSERT INTO q_nodes VALUES (?, ?, ?, ?, ?, ?)",
            params![&node_b[..], 10_000i32, 450_000_000i32, b_tx as i64, b_ty as i64, "nodeB"],
        ).unwrap();

        let geom = geom_to_blob(&[(-10_000, 450_000_000), (10_000, 450_000_000)]);
        // One row per home tile — exactly what every low-memory producer does.
        for (tx, ty) in [(a_tx, a_ty), (b_tx, b_ty)] {
            conn.execute(
                "INSERT INTO q_edges VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    0i64, 0i64, &node_a[..], &node_b[..], &edge_ab[..],
                    geom.as_slice(), 15i64, 3i64, 3i64, 0i64,
                    tx as i64, ty as i64, xyz_to_tile_id(tile_zoom, tx, ty) as i64, "edgeAB",
                ],
            ).unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        tile_from_duckdb(&conn, tile_zoom, dir.path(), "test", "test", false, false).unwrap();

        // Decode each tile payload into its OWN fresh TileLoader rather than merging
        // both into one Graph. TileLoader dedupes NODES globally by stable_id (a
        // separate, pre-existing, intentional asymmetry vs. segments — nodes get one
        // canonical global NodeId; segments get a fresh SegmentId per occurrence with
        // no dedup), so loading both tiles into one shared graph would make the second
        // tile's is_boundary view silently overwrite the first's. Inspecting each tile
        // in isolation observes exactly what that tile's payload actually says, which
        // is the property this test is pinning down.
        let archive = crate::find_pmtiles_in_dir(dir.path()).unwrap();
        let mut reader = crate::merge::PmtilesReader::open(&archive).unwrap();
        let mut per_tile_graphs = Vec::new();
        while let Some((_, bytes)) = reader.next_tile().unwrap() {
            let mut loader = openlr_provider::TileLoader::new();
            loader.load_tile_at(tile_zoom, 0, 0, &bytes).unwrap();
            per_tile_graphs.push(loader.graph);
        }
        assert_eq!(per_tile_graphs.len(), 2, "the boundary-spanning edge must produce exactly 2 tiles");

        // The edge must be present, with correct attributes, in BOTH tile payloads.
        for graph in &per_tile_graphs {
            let matching: Vec<_> = graph.segments.values().filter(|s| s.stable_id == "edgeAB").collect();
            assert_eq!(matching.len(), 1, "edgeAB must be present exactly once in each tile it's binned into");
            assert_eq!(matching[0].frc, 3);
            assert_eq!(matching[0].fow, 3);
        }

        // Each tile must see exactly one of {A, B} as boundary (the one that ISN'T
        // this tile's own home node) and the other as local.
        for graph in &per_tile_graphs {
            let a = graph.nodes.values().find(|n| n.stable_id == "nodeA").expect("node A must be present");
            let b = graph.nodes.values().find(|n| n.stable_id == "nodeB").expect("node B must be present");
            assert_ne!(a.is_boundary, b.is_boundary, "in any one tile, exactly one endpoint is local and the other is boundary");
        }
    }
}
