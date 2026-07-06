use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{info, info_span, warn};

use crate::{
    extent::Bbox,
    http::Client,
    osm_schema::OsmSchemaMapping,
    partition,
    schema::SchemaMapping,
};

// ── DuckDB temp-dir RAII guard ────────────────────────────────────────────────

/// Removes a DuckDB spill directory when dropped.
///
/// Created only when the pipeline allocates the default temp dir (i.e. the
/// caller did not supply `--duckdb-temp-dir`).  Ensures cleanup happens on
/// both success and early-return-via-`?` failure paths.
pub(crate) struct TempDirGuard(pub PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ── OSM PBF build path ────────────────────────────────────────────────────────

/// Build a PMTiles archive from a local OSM PBF file.
///
/// Skips the Overture-specific adapt/split/restrictions steps; instead calls
/// `osm_extract::extract` + `osm_adapt::adapt` which produce split edges,
/// nodes, and restrictions directly from OSM tags.
pub async fn run_osm(
    pbf_path:         &Path,
    extent_spec:      &str,
    bbox:             Option<Bbox>,
    osm_schema:       &OsmSchemaMapping,
    output:           &Path,
    tile_zoom:        u8,
    low_memory:       bool,
    compress:         bool,
    duckdb_memory_mb: Option<u64>,
    duckdb_temp_dir:  Option<&Path>,
    show_progress:    bool,
) -> Result<()> {
    std::fs::create_dir_all(output)?;
    let t0 = Instant::now();

    let extent_slug  = crate::extent::extent_slug(extent_spec);
    let pbf_stem     = pbf_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("osm");
    // Strip compression suffix if present (e.g. "new-zealand-latest.osm.pbf" → "new-zealand-latest")
    let release_label = pbf_stem.trim_end_matches(".osm");

    info!(
        pbf   = %pbf_path.display(),
        extent = %extent_slug,
        output = %output.display(),
        "OSM build started"
    );

    // Low-memory path: hand off entirely to the DuckDB-backed pipeline.
    if low_memory {
        let pbf_path    = pbf_path.to_path_buf();
        let schema_lm   = osm_schema.clone();
        let output_dir  = output.to_path_buf();
        let extent_slug = extent_slug.clone();
        let release_lm  = release_label.to_string();
        let tmp_dir     = duckdb_temp_dir.map(|p| p.to_path_buf());
        return tokio::task::spawn_blocking(move || {
            crate::osm_low_memory::run_pipeline(
                &pbf_path,
                bbox,
                &schema_lm,
                &output_dir,
                &extent_slug,
                &release_lm,
                tile_zoom,
                duckdb_memory_mb,
                tmp_dir.as_deref(),
                show_progress,
                compress,
            )
        })
        .await
        .context("osm_low_memory panicked")?;
    }

    // Step 1: extract ─────────────────────────────────────────────────────────
    let osm_data = {
        let _s = info_span!("osm_extract").entered();
        let pbf_path = pbf_path.to_path_buf();
        let schema_for_extract = osm_schema.clone();
        let data = tokio::task::spawn_blocking(move || {
            crate::osm_extract::extract(&pbf_path, bbox, &schema_for_extract)
        })
        .await
        .context("osm_extract panicked")??;
        info!(
            ways         = data.ways.len(),
            nodes        = data.nodes.len(),
            restrictions = data.restrictions.len(),
            elapsed_s    = t0.elapsed().as_secs_f32(),
            "OSM extract complete"
        );
        data
    };

    // Step 2: adapt + split ───────────────────────────────────────────────────
    let (edges, nodes, restrictions) = {
        let _s = info_span!("osm_adapt").entered();
        let (edges, nodes, restrictions) = tokio::task::spawn_blocking(move || {
            crate::osm_adapt::adapt(osm_data)
        })
        .await
        .context("osm_adapt panicked")?;

        let dir_fwd  = edges.iter().filter(|e| matches!(e.direction, openlr_graph::Direction::Forward)).count();
        let dir_bwd  = edges.iter().filter(|e| matches!(e.direction, openlr_graph::Direction::Backward)).count();
        let dir_both = edges.iter().filter(|e| matches!(e.direction, openlr_graph::Direction::Both)).count();
        info!(
            edges        = edges.len(),
            nodes        = nodes.len(),
            restrictions = restrictions.len(),
            dir_forward  = dir_fwd,
            dir_backward = dir_bwd,
            dir_both,
            elapsed_s    = t0.elapsed().as_secs_f32(),
            "OSM adapt complete"
        );
        (edges, nodes, restrictions)
    };

    // Step 3: quantize ────────────────────────────────────────────────────────
    let (q_edges, q_nodes) = {
        let _s = info_span!("quantize").entered();
        let (qe, qn) = tokio::task::spawn_blocking(move || {
            crate::quantize::quantize(edges, nodes)
        })
        .await
        .context("quantize panicked")?;
        info!(
            edges     = qe.len(),
            nodes     = qn.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "quantize complete"
        );
        (qe, qn)
    };

    // Step 4: tile and write PMTiles ──────────────────────────────────────────
    {
        let _s = info_span!("tile").entered();
        info!(tile_zoom, edges = q_edges.len(), restrictions = restrictions.len(), "tiling");
        let output_dir    = output.to_path_buf();
        let release_label = release_label.to_string();
        let extent_slug   = extent_slug.clone();
        tokio::task::spawn_blocking(move || {
            crate::tile::write_tiles(
                q_edges, q_nodes, restrictions,
                tile_zoom, &output_dir, &release_label, &extent_slug,
                low_memory, compress,
            )
        })
        .await
        .context("tile panicked")??;
    }

    info!(
        elapsed_s = t0.elapsed().as_secs_f32(),
        output    = %output.display(),
        "OSM build complete"
    );
    Ok(())
}

// ── Generic GeoJSONL build path ───────────────────────────────────────────────

/// Build a PMTiles archive from a GeoJSONL(.gz) road network file or directory.
///
/// Bypasses adapt and split (data arrives pre-attributed and pre-split); goes
/// straight from extract → quantize → tile.
pub async fn run_generic(
    roads_path:        &Path,
    restrictions_path: Option<&Path>,
    label:             &str,
    extent_spec:       &str,
    output:            &Path,
    tile_zoom:         u8,
    low_memory:        bool,
    compress:          bool,
    duckdb_memory_mb:  Option<u64>,
    duckdb_temp_dir:   Option<&Path>,
    show_progress:     bool,
) -> Result<()> {
    std::fs::create_dir_all(output)?;
    let t0 = Instant::now();

    let extent_slug = crate::extent::extent_slug(extent_spec);

    info!(
        roads       = %roads_path.display(),
        extent      = %extent_slug,
        output      = %output.display(),
        label,
        "generic build started"
    );

    // Low-memory path: hand off entirely to the DuckDB-backed pipeline.
    if low_memory {
        let roads       = roads_path.to_path_buf();
        let restr       = restrictions_path.map(|p| p.to_path_buf());
        let out_dir     = output.to_path_buf();
        let ext_slug    = extent_slug.clone();
        let label_owned = label.to_string();
        let tmp_dir     = duckdb_temp_dir.map(|p| p.to_path_buf());
        tokio::task::spawn_blocking(move || {
            crate::generic_low_memory::run_pipeline(
                &roads,
                restr.as_deref(),
                &out_dir,
                &ext_slug,
                tile_zoom,
                duckdb_memory_mb,
                tmp_dir.as_deref(),
                show_progress,
                compress,
            )
        })
        .await
        .context("generic_low_memory panicked")??;

        // Patch --label into manifest (same as in-memory path below).
        if !label_owned.is_empty() {
            let manifest_path = output.join("manifest.json");
            if let Ok(text) = std::fs::read_to_string(&manifest_path) {
                if let Ok(mut map) =
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
                {
                    map.insert(
                        "external_id_label".to_string(),
                        serde_json::Value::String(label_owned.clone()),
                    );
                    if let Ok(updated) =
                        serde_json::to_string_pretty(&serde_json::Value::Object(map))
                    {
                        let _ = std::fs::write(&manifest_path, updated);
                    }
                }
            }
        }

        info!(
            elapsed_s = t0.elapsed().as_secs_f32(),
            output    = %output.display(),
            "generic build complete"
        );
        return Ok(());
    }

    // Step 1: extract ─────────────────────────────────────────────────────────
    let (edges, nodes, seg_to_to_int) = {
        let _s = info_span!("generic_extract").entered();
        let roads_path = roads_path.to_path_buf();
        let (edges, nodes, seg_to_to_int) = tokio::task::spawn_blocking(move || {
            crate::generic_extract::extract(&roads_path)
        })
        .await
        .context("generic_extract panicked")??;
        info!(
            edges     = edges.len(),
            nodes     = nodes.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "generic extract complete"
        );
        (edges, nodes, seg_to_to_int)
    };

    // Step 2: restrictions (optional) ─────────────────────────────────────────
    let restrictions = if let Some(csv_path) = restrictions_path {
        let _s = info_span!("restrictions").entered();
        let csv_path = csv_path.to_path_buf();
        let r = tokio::task::spawn_blocking(move || {
            crate::generic_extract::read_restrictions_csv(&csv_path, &seg_to_to_int)
        })
        .await
        .context("restrictions panicked")??;
        info!(count = r.len(), elapsed_s = t0.elapsed().as_secs_f32(), "restrictions complete");
        r
    } else {
        vec![]
    };

    // Step 3: quantize ────────────────────────────────────────────────────────
    let (q_edges, q_nodes) = {
        let _s = info_span!("quantize").entered();
        let (qe, qn) = tokio::task::spawn_blocking(move || {
            crate::quantize::quantize(edges, nodes)
        })
        .await
        .context("quantize panicked")?;
        info!(
            edges     = qe.len(),
            nodes     = qn.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "quantize complete"
        );
        (qe, qn)
    };

    // Step 4: tile and write PMTiles ──────────────────────────────────────────
    {
        let _s = info_span!("tile").entered();
        info!(tile_zoom, edges = q_edges.len(), restrictions = restrictions.len(), "tiling");
        let output_dir   = output.to_path_buf();
        let extent_slug2 = extent_slug.clone();
        tokio::task::spawn_blocking(move || {
            crate::tile::write_tiles(
                q_edges, q_nodes, restrictions,
                tile_zoom, &output_dir, "generic", &extent_slug2,
                low_memory, compress,
            )
        })
        .await
        .context("tile panicked")??;
    }

    // Patch label into manifest if provided.
    if !label.is_empty() {
        let manifest_path = output.join("manifest.json");
        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
            if let Ok(mut map) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
            {
                map.insert(
                    "external_id_label".to_string(),
                    serde_json::Value::String(label.to_string()),
                );
                if let Ok(updated) =
                    serde_json::to_string_pretty(&serde_json::Value::Object(map))
                {
                    let _ = std::fs::write(&manifest_path, updated);
                }
            }
        }
    }

    info!(
        elapsed_s = t0.elapsed().as_secs_f32(),
        output    = %output.display(),
        "generic build complete"
    );
    Ok(())
}

// ── Canonical DuckDB build path ───────────────────────────────────────────────

/// Build a PMTiles archive from an existing DuckDB file already populated per
/// pipeline/schema/canonical_schema.sql. Always runs the DuckDB-native path —
/// there's no separate in-memory variant, since the whole point of this entry
/// point is supporting producers too large to hold in memory at all.
pub async fn run_canonical(
    canonical_db_path: &Path,
    label:             &str,
    extent_spec:       &str,
    output:            &Path,
    tile_zoom:         u8,
    compress:          bool,
    duckdb_memory_mb:  Option<u64>,
    duckdb_temp_dir:   Option<&Path>,
    show_progress:     bool,
) -> Result<()> {
    std::fs::create_dir_all(output)?;
    let t0 = Instant::now();

    let extent_slug = crate::extent::extent_slug(extent_spec);
    let release_label = if label.is_empty() { "canonical" } else { label };

    info!(
        canonical_db = %canonical_db_path.display(),
        extent       = %extent_slug,
        output       = %output.display(),
        "canonical build started"
    );

    let db_path      = canonical_db_path.to_path_buf();
    let out_dir      = output.to_path_buf();
    let ext_slug     = extent_slug.clone();
    let release_owned = release_label.to_string();
    let tmp_dir      = duckdb_temp_dir.map(|p| p.to_path_buf());
    tokio::task::spawn_blocking(move || {
        crate::canonical_low_memory::run_pipeline(
            &db_path,
            &out_dir,
            &ext_slug,
            &release_owned,
            tile_zoom,
            duckdb_memory_mb,
            tmp_dir.as_deref(),
            show_progress,
            compress,
        )
    })
    .await
    .context("canonical_low_memory panicked")??;

    info!(
        elapsed_s = t0.elapsed().as_secs_f32(),
        output    = %output.display(),
        "canonical build complete"
    );
    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn run(
    release:           &str,
    extent_spec:       &str,
    bbox:              Option<Bbox>,
    schema:            &SchemaMapping,
    output:            &Path,
    client:            &Client,
    fetch_concurrency: usize,
    tile_zoom:         u8,
    ram_gb_override:   Option<f64>,
    bytes_per_segment: u64,
    low_memory:        bool,
    compress:          bool,
) -> Result<()> {
    std::fs::create_dir_all(output)?;

    // Detect RAM and decide how many partitions are needed.
    let available  = partition::available_ram_bytes();
    let budget     = partition::ram_budget_bytes(available, ram_gb_override);
    let partitions = partition::compute_partitions(bbox, budget, bytes_per_segment);

    info!(
        available_ram_gb  = format!("{:.1}", available  as f64 / 1e9),
        budget_gb         = format!("{:.1}", budget     as f64 / 1e9),
        partitions        = partitions.len(),
        "build plan"
    );

    let extent_slug = crate::extent::extent_slug(extent_spec);
    let safe_release    = release.replace('.', "-");
    let archive_name    = format!("openlrlens-{extent_slug}-{safe_release}.pmtiles");
    let final_pmtiles   = output.join(&archive_name);

    if partitions.len() == 1 {
        // ── Single-shot ────────────────────────────────────────────────────────
        run_partition(
            release, bbox, schema, output, client,
            fetch_concurrency, tile_zoom, &extent_slug, low_memory, compress,
        )
        .await
    } else {
        // ── Multi-partition: process each piece then merge ─────────────────────
        let part_dir = output.join("_parts");
        std::fs::create_dir_all(&part_dir)?;

        let mut part_pmtiles: Vec<PathBuf> = Vec::with_capacity(partitions.len());

        for (i, part_bbox) in partitions.iter().enumerate() {
            let part_slug = format!("part-{i:04}");
            let part_out  = part_dir.join(&part_slug);
            std::fs::create_dir_all(&part_out)?;

            info!(
                partition = i + 1,
                total     = partitions.len(),
                west  = part_bbox.west,
                south = part_bbox.south,
                east  = part_bbox.east,
                north = part_bbox.north,
                "processing partition"
            );

            run_partition(
                release, Some(*part_bbox), schema, &part_out, client,
                fetch_concurrency, tile_zoom, &part_slug, low_memory, compress,
            )
            .await?;

            match find_pmtiles(&part_out) {
                Some(p) => part_pmtiles.push(p),
                None    => warn!(dir = %part_out.display(), "no .pmtiles found in partition dir"),
            }
        }

        // Merge all partition archives into the final archive.
        {
            let _s = info_span!("merge").entered();
            info!(
                archives = part_pmtiles.len(),
                output   = %final_pmtiles.display(),
                "merging partition archives"
            );
            crate::merge::merge_pmtiles(&part_pmtiles, &final_pmtiles, tile_zoom)?;
        }

        // Write a single manifest for the merged archive.
        write_top_manifest(output, &archive_name, release, &extent_slug, tile_zoom)?;

        // Clean up partition working directories.
        if let Err(e) = std::fs::remove_dir_all(&part_dir) {
            warn!(error = %e, "could not remove partition working dir");
        }

        info!(output = %final_pmtiles.display(), "multi-partition build complete");
        Ok(())
    }
}

// ── Single-partition pipeline (the core of the original build::run) ───────────

async fn run_partition(
    release:           &str,
    bbox:              Option<Bbox>,
    schema:            &SchemaMapping,
    output_dir:        &Path,
    client:            &Client,
    fetch_concurrency: usize,
    tile_zoom:         u8,
    extent_slug:       &str,
    low_memory:        bool,
    compress:          bool,
) -> Result<()> {
    let t0 = Instant::now();

    info!(
        release,
        extent   = %extent_slug,
        output   = %output_dir.display(),
        "partition started"
    );

    // Step 1: extract ─────────────────────────────────────────────────────────
    let raw_segments = {
        let _s = info_span!("extract", release, extent = %extent_slug).entered();
        info!("extracting segments from Overture parquet");
        let segs = crate::extract::extract_segments(release, bbox, client, fetch_concurrency)
            .await
            .context("extract")?;
        info!(count = segs.len(), elapsed_s = t0.elapsed().as_secs_f32(), "extract complete");
        segs
    };

    // Step 2: adapt ───────────────────────────────────────────────────────────
    let adapted = {
        let _s = info_span!("adapt").entered();
        info!("adapting class/subclass/road_flags → frc/fow/direction");
        let schema = schema.clone();
        let adapted = tokio::task::spawn_blocking(move || {
            crate::adapt::adapt(raw_segments, &schema)
        })
        .await
        .context("adapt panicked")?;
        let dir_fwd  = adapted.iter().filter(|s| matches!(s.direction, openlr_graph::Direction::Forward)).count();
        let dir_bwd  = adapted.iter().filter(|s| matches!(s.direction, openlr_graph::Direction::Backward)).count();
        let dir_both = adapted.iter().filter(|s| matches!(s.direction, openlr_graph::Direction::Both)).count();
        let excluded = adapted.iter().filter(|s| !s.vehicular).count();
        info!(count = adapted.len(), dir_forward = dir_fwd, dir_backward = dir_bwd, dir_both,
              non_vehicular_excluded = excluded, elapsed_s = t0.elapsed().as_secs_f32(), "adapt complete");
        adapted
    };

    // Filter non-vehicular segments before restrictions and split.
    let adapted: Vec<_> = adapted.into_iter().filter(|s| s.vehicular).collect();

    // Step 5 (pre-split): restrictions ────────────────────────────────────────
    let restrictions = {
        let _s = info_span!("restrictions").entered();
        info!("flattening prohibited_transitions → turn-restriction table");
        let r = crate::restrictions::flatten(&adapted);
        info!(count = r.len(), elapsed_s = t0.elapsed().as_secs_f32(), "restrictions complete");
        r
    };

    // Build the set of connector IDs that are endpoints (at ≈ 0 or 1) of vehicular segments.
    // Interior connectors not in this set connect only to non-vehicular ways and are skipped.
    let vehicular_endpoints: HashSet<String> = adapted.iter()
        .flat_map(|s| s.connectors.iter())
        .filter(|c| c.at <= 1e-9 || c.at >= 1.0 - 1e-9)
        .map(|c| c.connector_id.clone())
        .collect();

    // Steps 3+4: split at interior connectors ─────────────────────────────────
    let (edges, nodes) = {
        let _s = info_span!("split").entered();
        info!("splitting segments at interior connectors");
        let (edges, nodes) = tokio::task::spawn_blocking(move || {
            crate::split::split(adapted, &vehicular_endpoints)
        })
        .await
        .context("split panicked")?;
        info!(
            edges = edges.len(),
            nodes = nodes.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "split complete"
        );
        (edges, nodes)
    };

    // Step 6: quantize ────────────────────────────────────────────────────────
    let (q_edges, q_nodes) = {
        let _s = info_span!("quantize").entered();
        info!("quantizing geometry to 1e-7 degree grid");
        let (qe, qn) = tokio::task::spawn_blocking(move || {
            crate::quantize::quantize(edges, nodes)
        })
        .await
        .context("quantize panicked")?;
        info!(
            edges = qe.len(),
            nodes = qn.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "quantize complete"
        );
        (qe, qn)
    };

    // Step 7: tile and write PMTiles ──────────────────────────────────────────
    {
        let _s = info_span!("tile").entered();
        info!(
            tile_zoom,
            edges        = q_edges.len(),
            restrictions = restrictions.len(),
            "tiling and writing PMTiles archive"
        );
        let output_dir   = output_dir.to_path_buf();
        let release      = release.to_string();
        let extent_slug  = extent_slug.to_string();
        tokio::task::spawn_blocking(move || {
            crate::tile::write_tiles(
                q_edges, q_nodes, restrictions,
                tile_zoom, &output_dir, &release, &extent_slug,
                low_memory, compress,
            )
        })
        .await
        .context("tile panicked")??;
        info!(elapsed_s = t0.elapsed().as_secs_f32(), "partition complete");
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Find the first `.pmtiles` file in `dir`.
fn find_pmtiles(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.find_map(|e| {
        let p = e.ok()?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("pmtiles") {
            Some(p)
        } else {
            None
        }
    })
}

/// Write the manifest for the final merged archive (overwrites any per-partition manifest).
pub(crate) fn write_top_manifest(
    output_dir:    &Path,
    archive_name:  &str,
    release:       &str,
    extent_slug:   &str,
    tile_zoom:     u8,
) -> Result<()> {
    // Reuse tile::write_manifest logic via a small duplicate — avoids making it pub.
    let manifest = serde_json::json!({
        "archive":   archive_name,
        "release":   release,
        "extent":    extent_slug,
        "tile_zoom": tile_zoom,
    });
    let path = output_dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

// ── Parity tests: in-memory vs low-memory OSM path ────────────────────────────
//
// osm_extract+osm_adapt+quantize+tile::write_tiles (in-memory) and
// osm_low_memory::run_pipeline (DuckDB-backed) are two independent
// implementations of the same pipeline stages, kept in sync by hand. These
// tests prove they agree on *decoded semantic content*, not on raw archive
// bytes — byte-identity turned out to be too strict a bar: the in-memory
// path's rayon-parallel PBF scan (osm_extract.rs's par_map_reduce) has
// run-to-run ordering non-determinism of its own, confirmed empirically (two
// back-to-back runs of the identical code on the identical input produced
// different SHA-256 archive hashes at full NZ scale). That non-determinism
// only ever reshuffles arbitrary tile-local array indices; it never changes a
// persisted stable_id, so it's benign for correctness but makes raw-byte
// comparison meaningless. Comparing decoded segments/nodes/restrictions as
// order-independent sets, keyed by stable_id, is the correct bar.

#[cfg(test)]
mod osm_parity_tests {
    use super::*;
    use crate::osm_schema::OsmSchemaMapping;
    use openlr_provider::TileLoader;

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/wellington-tiny.osm.pbf")
    }

    /// Decode every tile of the archive in `dir` into a Graph. The (z, x, y)
    /// fed to `load_tile_at` is a synthetic, arbitrary-but-unique counter, not
    /// the tile's real Hilbert tile_id — cross-tile stitching and restriction
    /// resolution both key on stable_id strings, not on (z, x, y), so this is
    /// sufficient to reconstruct the full graph for content comparison.
    fn load_graph(dir: &Path) -> openlr_graph::Graph {
        let archive = crate::find_pmtiles_in_dir(dir).expect("build must produce a .pmtiles archive");
        let mut reader = crate::merge::PmtilesReader::open(&archive).expect("open PMTiles archive");
        let mut loader = TileLoader::new();
        let mut i: u32 = 0;
        while let Some((_, bytes)) = reader.next_tile().expect("read tile") {
            loader.load_tile_at(12, i, 0, &bytes).expect("load tile into graph");
            i += 1;
        }
        loader.graph
    }

    /// Sorted, order-independent snapshot of a graph's decodable content —
    /// everything an OpenLR decode actually depends on, keyed by the stable ids
    /// that are supposed to survive a rebuild (Invariant 2), not by whatever
    /// arbitrary tile-local array index a segment/node happened to land at.
    #[derive(Debug, PartialEq)]
    struct GraphSnapshot {
        segments: Vec<(String, String, String, u8, u8, String, i64, Vec<(i64, i64)>)>,
        nodes: Vec<(String, i64, i64)>,
        restrictions: Vec<(String, String, String)>,
    }

    fn snapshot(graph: &openlr_graph::Graph) -> GraphSnapshot {
        let node_stable = |id: openlr_graph::NodeId| {
            graph.nodes.get(&id).expect("segment references a node not in the graph").stable_id.clone()
        };

        let mut segments: Vec<_> = graph
            .segments
            .values()
            .map(|s| {
                (
                    s.stable_id.clone(),
                    node_stable(s.start_node),
                    node_stable(s.end_node),
                    s.frc,
                    s.fow,
                    format!("{:?}", s.direction),
                    (s.length_m * 100.0).round() as i64, // cm precision, matching the tile's length_cm
                    s.geometry
                        .iter()
                        .map(|&(lon, lat)| ((lon * 1e7).round() as i64, (lat * 1e7).round() as i64))
                        .collect(),
                )
            })
            .collect();
        segments.sort_by(|a, b| a.0.cmp(&b.0));

        let mut nodes: Vec<_> = graph
            .nodes
            .values()
            .map(|n| (n.stable_id.clone(), (n.lon * 1e7).round() as i64, (n.lat * 1e7).round() as i64))
            .collect();
        nodes.sort_by(|a, b| a.0.cmp(&b.0));

        let mut restrictions: Vec<_> = graph
            .restrictions()
            .iter()
            .map(|r| {
                (
                    graph.segments[&r.from_seg].stable_id.clone(),
                    node_stable(r.via_node),
                    graph.segments[&r.to_seg].stable_id.clone(),
                )
            })
            .collect();
        restrictions.sort();

        GraphSnapshot { segments, nodes, restrictions }
    }

    /// Real, small (37 KB) OSM extract (osmium, `simple` strategy, central
    /// Wellington) with ways, intersections, and turn restrictions — committed
    /// to fixtures/ specifically for this test. Runs both pipelines end to end
    /// with an actual bbox filter applied (so each implementation's own
    /// separate bbox-filtering code — osm_extract's inline filter vs
    /// osm_low_memory::apply_bbox_filter — gets exercised too), then compares
    /// decoded graph content.
    #[tokio::test]
    async fn in_memory_and_low_memory_agree_on_decoded_content() {
        let pbf = fixture_path();
        assert!(pbf.exists(), "fixture missing: {}", pbf.display());

        let schema = OsmSchemaMapping::load_default();
        let bbox = crate::extent::resolve("174.7765,-41.2915,174.7780,-41.2900").unwrap();
        let extent = "174.7765,-41.2915,174.7780,-41.2900";

        let out_inmem = tempfile::tempdir().unwrap();
        let out_lowmem = tempfile::tempdir().unwrap();

        run_osm(&pbf, extent, bbox, &schema, out_inmem.path(), 12, false, false, None, None, false)
            .await
            .expect("in-memory OSM build must succeed");
        run_osm(&pbf, extent, bbox, &schema, out_lowmem.path(), 12, true, false, None, None, false)
            .await
            .expect("low-memory OSM build must succeed");

        let snap_inmem = snapshot(&load_graph(out_inmem.path()));
        let snap_lowmem = snapshot(&load_graph(out_lowmem.path()));

        assert!(!snap_inmem.segments.is_empty(), "fixture must produce at least one segment");
        assert_eq!(snap_inmem, snap_lowmem, "in-memory and low-memory OSM paths must decode to identical graph content");
    }

    /// Same check at full scale against a real regional extract, for extra
    /// confidence on code paths the tiny fixture can't exercise (the chunked
    /// intersection-node GROUP BY, cursor-based way streaming, memory-limit
    /// lowering/restoring around the adapt stage). Not run by default — needs
    /// a multi-hundred-MB file this repo doesn't commit; run explicitly with
    /// `cargo test -- --ignored` after placing new-zealand-latest.osm.pbf (or
    /// any Geofabrik regional extract) at the repo root.
    #[tokio::test]
    #[ignore = "needs a large local .osm.pbf fixture; see doc comment"]
    async fn in_memory_and_low_memory_agree_at_full_regional_scale() {
        let pbf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../new-zealand-latest.osm.pbf");
        assert!(
            pbf.exists(),
            "place a regional .osm.pbf at {} to run this test",
            pbf.display()
        );

        let schema = OsmSchemaMapping::load_default();
        let bbox = crate::extent::resolve("NZ").unwrap();

        let out_inmem = tempfile::tempdir().unwrap();
        let out_lowmem = tempfile::tempdir().unwrap();

        run_osm(&pbf, "NZ", bbox, &schema, out_inmem.path(), 12, false, false, None, None, true)
            .await
            .expect("in-memory OSM build must succeed");
        run_osm(&pbf, "NZ", bbox, &schema, out_lowmem.path(), 12, true, false, None, None, true)
            .await
            .expect("low-memory OSM build must succeed");

        let snap_inmem = snapshot(&load_graph(out_inmem.path()));
        let snap_lowmem = snapshot(&load_graph(out_lowmem.path()));

        assert!(!snap_inmem.segments.is_empty(), "regional fixture must produce at least one segment");
        assert_eq!(
            snap_inmem, snap_lowmem,
            "in-memory and low-memory OSM paths must decode to identical graph content at regional scale"
        );
    }
}
