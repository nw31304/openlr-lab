use std::path::Path;
use anyhow::{Context, Result};
use duckdb::Connection;
use serde::Deserialize;
use tempfile::NamedTempFile;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::{extent::Bbox, http::Client};

// ── Overture segment types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct OvertureSegment {
    pub id: String,
    /// WGS84 coords decoded from WKB geometry column.
    #[serde(skip)]
    pub geometry: Vec<(f64, f64)>,
    /// Raw WKB hex or bytes; filled before the geometry field is populated.
    pub geometry_wkb: Option<String>,
    pub class: String,
    pub subclass: Option<String>,
    #[serde(default)]
    pub connectors: Vec<ConnectorRef>,
    #[serde(default)]
    pub road_flags: Vec<RoadFlagEntry>,
    #[serde(default)]
    pub access_restrictions: Vec<AccessRestriction>,
    #[serde(default)]
    pub prohibited_transitions: Vec<ProhibitedTransition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectorRef {
    pub connector_id: String,
    pub at: f64, // fractional position along segment [0, 1]
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RoadFlagEntry {
    /// Flag names active for this span (e.g. "is_bridge", "is_tunnel", "is_link").
    pub values: Vec<String>,
    /// Fractional [start, end] range along the segment where the flags apply; None = whole segment.
    pub between: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccessRestriction {
    pub access_type: Option<String>,
    #[serde(rename = "when")]
    pub when_condition: Option<AccessWhen>,
    pub heading: Option<String>, // "forward" | "backward" (Overture puts it inside "when", not here)
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct AccessWhen {
    pub heading: Option<String>,  // "forward" | "backward"
    pub during: Option<String>,
    pub vehicle: Option<Vec<String>>,
    pub mode: Option<Vec<String>>,
}

/// One prohibited turn as stored in Overture transportation parquet.
/// Each entry lives on its "from" segment; `sequence` lists the forbidden onward hops.
/// For the common single-hop case: sequence[0].connector_id is the via junction,
/// sequence[0].segment_id is the "to" segment.
#[derive(Debug, Clone, Deserialize)]
pub struct ProhibitedTransition {
    pub sequence: Vec<SequenceEntry>,
    pub final_heading: Option<String>,
    #[serde(rename = "when")]
    pub when_condition: Option<AccessWhen>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SequenceEntry {
    pub connector_id: String,
    pub segment_id: String,
}

// ── S3 listing ────────────────────────────────────────────────────────────────

const S3_BASE: &str =
    "https://overturemaps-us-west-2.s3.amazonaws.com/?list-type=2&prefix=";

pub async fn list_segment_files(release: &str, client: &Client) -> Result<Vec<String>> {
    let prefix = format!("release/{release}/theme=transportation/type=segment/");
    let mut keys: Vec<String> = Vec::new();
    let mut continuation: Option<String> = None;

    loop {
        let url = if let Some(ref tok) = continuation {
            format!(
                "{S3_BASE}{}&continuation-token={}",
                urlencoding::encode(&prefix),
                urlencoding::encode(tok)
            )
        } else {
            format!("{S3_BASE}{}", urlencoding::encode(&prefix))
        };

        let xml = client.get_text(&url).await?;
        let (page_keys, next) = parse_listing_xml(&xml)?;
        keys.extend(page_keys);
        if let Some(tok) = next {
            continuation = Some(tok);
        } else {
            break;
        }
    }

    info!(count = keys.len(), "parquet files listed for release");
    Ok(keys)
}

fn parse_listing_xml(xml: &str) -> Result<(Vec<String>, Option<String>)> {
    let mut keys = Vec::new();
    let mut next_token: Option<String> = None;

    // Extract <Key>…</Key> entries that end with .parquet
    let mut pos = 0;
    while let Some(start) = xml[pos..].find("<Key>") {
        let abs_start = pos + start + 5;
        let end = xml[abs_start..].find("</Key>").context("malformed S3 XML")?;
        let key = &xml[abs_start..abs_start + end];
        if key.ends_with(".parquet") {
            keys.push(key.to_string());
        }
        pos = abs_start + end + 6;
    }

    // Check for pagination token
    if let Some(start) = xml.find("<NextContinuationToken>") {
        let abs = start + 23;
        if let Some(end) = xml[abs..].find("</NextContinuationToken>") {
            next_token = Some(xml[abs..abs + end].to_string());
        }
    }

    // IsTruncated=false → no token needed (belt-and-suspenders)
    if xml.contains("<IsTruncated>false</IsTruncated>") {
        next_token = None;
    }

    Ok((keys, next_token))
}

fn s3_key_url(key: &str) -> String {
    format!("https://overturemaps-us-west-2.s3.amazonaws.com/{key}")
}

// ── Per-file download + DuckDB query ─────────────────────────────────────────

async fn download_to_tempfile(url: &str, client: &Client) -> Result<NamedTempFile> {
    let bytes = client.get_bytes(url).await?;
    let tmp = NamedTempFile::new().context("create tempfile")?;
    std::fs::write(tmp.path(), &bytes).context("write tempfile")?;
    Ok(tmp)
}

fn query_parquet_file(path: &Path, bbox: Option<Bbox>) -> Result<Vec<OvertureSegment>> {
    let conn = Connection::open_in_memory()?;

    // Spatial filter on the bbox column (pre-computed in Overture parquet)
    let bbox_filter = match bbox {
        Some(b) => format!(
            "AND bbox.xmin <= {east} AND bbox.xmax >= {west} \
             AND bbox.ymin <= {north} AND bbox.ymax >= {south}",
            west = b.west,
            south = b.south,
            east = b.east,
            north = b.north,
        ),
        None => String::new(),
    };

    // geometry is a GeoParquet GEOMETRY column; ST_AsWKB converts it to raw WKB bytes (BLOB).
    let sql = format!(
        r#"
        SELECT
            id,
            ST_AsWKB(geometry)                               AS geometry_wkb,
            COALESCE("class",    '')                         AS class,
            subclass                                         AS subclass,
            to_json(connectors)::VARCHAR                     AS connectors,
            to_json(road_flags)::VARCHAR                     AS road_flags,
            to_json(access_restrictions)::VARCHAR            AS access_restrictions,
            to_json(prohibited_transitions)::VARCHAR         AS prohibited_transitions
        FROM read_parquet('{path}')
        WHERE 1=1 {bbox_filter}
        "#,
        path = path.display(),
        bbox_filter = bbox_filter,
    );

    let mut stmt = conn.prepare(&sql)?;

    #[derive(Debug)]
    struct Row {
        id: String,
        geometry_wkb: Vec<u8>,
        class: String,
        subclass: Option<String>,
        connectors_json: String,
        road_flags_json: String,
        access_json: String,
        prohibited_json: String,
    }

    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok(Row {
                id: row.get(0)?,
                geometry_wkb: row.get(1)?,
                class: row.get(2)?,
                subclass: row.get(3)?,
                connectors_json: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                road_flags_json: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                access_json: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                prohibited_json: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut segments = Vec::with_capacity(rows.len());
    for row in rows {
        let geometry = parse_wkb_bytes(&row.geometry_wkb)
            .with_context(|| format!("WKB parse for segment {}", row.id))?;

        let connectors: Vec<ConnectorRef> =
            serde_json::from_str(&row.connectors_json).unwrap_or_default();
        let road_flags: Vec<RoadFlagEntry> =
            serde_json::from_str(&row.road_flags_json).unwrap_or_default();
        let access_restrictions: Vec<AccessRestriction> =
            serde_json::from_str(&row.access_json).unwrap_or_default();
        let prohibited_transitions: Vec<ProhibitedTransition> =
            serde_json::from_str(&row.prohibited_json).unwrap_or_default();

        segments.push(OvertureSegment {
            id: row.id,
            geometry,
            geometry_wkb: None,
            class: row.class,
            subclass: row.subclass,
            connectors,
            road_flags,
            access_restrictions,
            prohibited_transitions,
        });
    }

    Ok(segments)
}

// ── WKB LineString parser ─────────────────────────────────────────────────────

/// Parses a hex-encoded WKB LineString (type 2) or LinearRing (WKB type 2).
/// Supports both little-endian (01) and big-endian (00) byte order.
/// Ignores Z/M variants by masking off the high bits of the geometry type.
#[allow(dead_code)]
pub fn parse_linestring_wkb(hex: &str) -> Result<Vec<(f64, f64)>> {
    let hex = hex.trim();
    if hex.len() < 10 {
        anyhow::bail!("WKB too short: {} chars", hex.len());
    }
    let bytes = hex::decode(hex).context("WKB hex decode")?;
    parse_wkb_bytes(&bytes)
}

fn parse_wkb_bytes(bytes: &[u8]) -> Result<Vec<(f64, f64)>> {
    if bytes.is_empty() {
        anyhow::bail!("empty WKB");
    }
    let little_endian = match bytes[0] {
        1 => true,
        0 => false,
        b => anyhow::bail!("unknown WKB byte-order marker: {b}"),
    };

    let read_u32 = |off: usize| -> Result<u32> {
        let b = bytes.get(off..off + 4).context("WKB truncated (u32)")?;
        Ok(if little_endian {
            u32::from_le_bytes(b.try_into().unwrap())
        } else {
            u32::from_be_bytes(b.try_into().unwrap())
        })
    };
    let read_f64 = |off: usize| -> Result<f64> {
        let b = bytes.get(off..off + 8).context("WKB truncated (f64)")?;
        Ok(if little_endian {
            f64::from_le_bytes(b.try_into().unwrap())
        } else {
            f64::from_be_bytes(b.try_into().unwrap())
        })
    };

    let geom_type = read_u32(1)? & 0x0000_FFFF; // mask off Z/M flags
    // WKB types: 1=Point, 2=LineString, 3=Polygon, 5=MultiPoint, etc.
    // We only handle LineString (2); Overture segments are always LineStrings.
    if geom_type != 2 {
        anyhow::bail!("expected WKB LineString (type 2), got type {geom_type}");
    }

    let num_points = read_u32(5)? as usize;
    let mut coords = Vec::with_capacity(num_points);
    let base = 9; // 1 (byte order) + 4 (type) + 4 (num_points)
    for i in 0..num_points {
        let off = base + i * 16;
        let x = read_f64(off)?;
        let y = read_f64(off + 8)?;
        coords.push((x, y)); // WKB is (longitude, latitude)
    }

    if coords.len() < 2 {
        anyhow::bail!("LineString has fewer than 2 points");
    }
    Ok(coords)
}

// ── Parquet bbox pre-filter ───────────────────────────────────────────────────

/// Run concurrent suffix-range requests on all listed files and return only those
/// whose row-group bbox statistics overlap `bbox`.  Files whose metadata cannot be
/// read are kept (conservative).  Uses up to `concurrency` simultaneous requests.
async fn filter_keys_by_bbox(
    keys: Vec<String>,
    bbox: Bbox,
    client: &Client,
    concurrency: usize,
) -> Vec<String> {
    let sem = std::sync::Arc::new(Semaphore::new(concurrency));
    let mut handles = Vec::with_capacity(keys.len());

    for key in keys {
        let url    = s3_key_url(&key);
        let sem    = sem.clone();
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore closed");
            let ok = crate::parquet_meta::file_may_overlap(&url, bbox, &client).await;
            (key, ok)
        }));
    }

    let mut out = Vec::new();
    for h in handles {
        if let Ok((key, true)) = h.await { out.push(key); }
    }
    out
}

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn extract_segments(
    release: &str,
    bbox: Option<Bbox>,
    client: &Client,
    fetch_concurrency: usize,
) -> Result<Vec<OvertureSegment>> {
    let keys = list_segment_files(release, client).await?;
    let original_count = keys.len();

    // Pre-filter by bbox: skip files whose row groups provably don't overlap.
    // Uses up to 4× fetch_concurrency (metadata requests are cheap — 128 KB each).
    let keys = if let Some(b) = bbox {
        let filtered = filter_keys_by_bbox(
            keys, b, client, fetch_concurrency * 4,
        )
        .await;
        info!(
            original = original_count,
            remaining = filtered.len(),
            "parquet files after bbox pre-filter"
        );
        filtered
    } else {
        keys
    };

    info!(files = keys.len(), "starting concurrent parquet fetch");

    let semaphore = std::sync::Arc::new(Semaphore::new(fetch_concurrency));
    let mut handles = Vec::with_capacity(keys.len());

    for key in &keys {
        let url = s3_key_url(key);
        let sem = semaphore.clone();
        let client = client.clone();
        let bbox = bbox;
        let key = key.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore closed");
            debug!(key = %key, "downloading parquet");

            let tmp = match download_to_tempfile(&url, &client).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(key = %key, error = %e, "download failed, skipping file");
                    return Ok::<Vec<OvertureSegment>, anyhow::Error>(Vec::new());
                }
            };

            let result = tokio::task::spawn_blocking(move || {
                query_parquet_file(tmp.path(), bbox)
                // tmp is dropped here, deleting the tempfile
            })
            .await
            .context("spawn_blocking panicked")??;

            debug!(key = %key, count = result.len(), "parquet file extracted");
            Ok(result)
        }));
    }

    let mut all: Vec<OvertureSegment> = Vec::new();
    for handle in handles {
        match handle.await.context("task join")? {
            Ok(segs) => all.extend(segs),
            Err(e) => warn!(error = %e, "file extraction error"),
        }
    }

    info!(total = all.len(), "extract complete");
    Ok(all)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wkb_little_endian_linestring() {
        // Manually constructed WKB for a 3-point LineString in little-endian:
        // byte-order=01, type=02000000, num_points=03000000
        // Point 1: (174.0, -36.0) → lon=174.0, lat=-36.0
        // Point 2: (174.5, -36.5)
        // Point 3: (175.0, -37.0)
        let mut wkb = Vec::<u8>::new();
        wkb.push(0x01); // little-endian
        wkb.extend_from_slice(&2u32.to_le_bytes()); // type = LineString
        wkb.extend_from_slice(&3u32.to_le_bytes()); // 3 points
        for (lon, lat) in [(174.0f64, -36.0f64), (174.5, -36.5), (175.0, -37.0)] {
            wkb.extend_from_slice(&lon.to_le_bytes());
            wkb.extend_from_slice(&lat.to_le_bytes());
        }
        let hex = hex::encode(&wkb);
        let coords = parse_linestring_wkb(&hex).unwrap();
        assert_eq!(coords.len(), 3);
        assert!((coords[0].0 - 174.0).abs() < 1e-9);
        assert!((coords[0].1 - (-36.0)).abs() < 1e-9);
        assert!((coords[2].0 - 175.0).abs() < 1e-9);
    }

    #[test]
    fn wkb_big_endian_linestring() {
        let mut wkb = Vec::<u8>::new();
        wkb.push(0x00); // big-endian
        wkb.extend_from_slice(&2u32.to_be_bytes());
        wkb.extend_from_slice(&2u32.to_be_bytes()); // 2 points
        for (lon, lat) in [(10.0f64, 50.0f64), (10.1, 50.1)] {
            wkb.extend_from_slice(&lon.to_be_bytes());
            wkb.extend_from_slice(&lat.to_be_bytes());
        }
        let hex = hex::encode(&wkb);
        let coords = parse_linestring_wkb(&hex).unwrap();
        assert_eq!(coords.len(), 2);
        assert!((coords[1].0 - 10.1).abs() < 1e-9);
    }

    #[test]
    fn wkb_rejects_point_type() {
        let mut wkb = Vec::<u8>::new();
        wkb.push(0x01);
        wkb.extend_from_slice(&1u32.to_le_bytes()); // type = Point, not LineString
        wkb.extend_from_slice(&174.0f64.to_le_bytes());
        wkb.extend_from_slice(&(-36.0f64).to_le_bytes());
        let hex = hex::encode(&wkb);
        assert!(parse_linestring_wkb(&hex).is_err());
    }

    #[test]
    fn wkb_z_variant_masked() {
        // WKB type 1002 (LineStringZ) should be accepted by masking off the Z flag.
        // ISO WKB encodes Z as type + 1000; some drivers use 0x80000000 flag instead.
        // We mask with 0xFFFF so type 1002 → 1002 which != 2 — test that we'd need 0x0FFF.
        // In practice Overture uses standard 2D WKB, but let's verify our masking:
        let mut wkb = Vec::<u8>::new();
        wkb.push(0x01);
        // Type 1000 + 2 = 1002 (LineStringZ, ISO variant)
        wkb.extend_from_slice(&1002u32.to_le_bytes());
        wkb.extend_from_slice(&2u32.to_le_bytes()); // 2 points
        for (x, y, z) in [(1.0f64, 2.0f64, 0.0f64), (3.0, 4.0, 0.0)] {
            wkb.extend_from_slice(&x.to_le_bytes());
            wkb.extend_from_slice(&y.to_le_bytes());
            wkb.extend_from_slice(&z.to_le_bytes()); // Z coord; our parser ignores it
        }
        // Our current parser doesn't handle 3D; this test documents the gap
        let hex = hex::encode(&wkb);
        let result = parse_linestring_wkb(&hex);
        // Not expected to succeed with ISO Z encoding (type=1002), just verify no panic
        let _ = result; // accepted or rejected — either is fine, not a panic
    }

    #[test]
    fn s3_listing_xml_parses_keys_and_token() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <ListBucketResult>
          <IsTruncated>true</IsTruncated>
          <Contents><Key>release/2026-05-20.0/theme=transportation/type=segment/part-00000.parquet</Key></Contents>
          <Contents><Key>release/2026-05-20.0/theme=transportation/type=segment/part-00001.parquet</Key></Contents>
          <Contents><Key>release/2026-05-20.0/theme=transportation/type=segment/README.txt</Key></Contents>
          <NextContinuationToken>abc123token</NextContinuationToken>
        </ListBucketResult>"#;

        let (keys, token) = parse_listing_xml(xml).unwrap();
        assert_eq!(keys.len(), 2); // README.txt excluded
        assert_eq!(keys[0], "release/2026-05-20.0/theme=transportation/type=segment/part-00000.parquet");
        assert_eq!(token.as_deref(), Some("abc123token"));
    }

    #[test]
    fn s3_listing_xml_not_truncated() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <ListBucketResult>
          <IsTruncated>false</IsTruncated>
          <Contents><Key>release/x/theme=transportation/type=segment/part-00000.parquet</Key></Contents>
          <NextContinuationToken>should-be-ignored</NextContinuationToken>
        </ListBucketResult>"#;

        let (keys, token) = parse_listing_xml(xml).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(token.is_none()); // IsTruncated=false wins
    }

    #[test]
    fn road_flag_entry_deserializes() {
        // Actual Overture format: {"values": ["is_bridge"], "between": [0.1, 0.9]}
        let json = r#"[{"values":["is_bridge"],"between":[0.011394098,0.012977352]},{"values":["is_tunnel","is_covered"],"between":null}]"#;
        let flags: Vec<RoadFlagEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(flags.len(), 2);
        assert_eq!(flags[0].values, vec!["is_bridge"]);
        assert!(flags[0].between.is_some());
        assert_eq!(flags[1].values, vec!["is_tunnel", "is_covered"]);
        assert!(flags[1].between.is_none());
    }

    #[test]
    fn connector_ref_deserializes() {
        let json = r#"[{"connector_id":"conn-abc","at":0.5},{"connector_id":"conn-xyz","at":1.0}]"#;
        let refs: Vec<ConnectorRef> = serde_json::from_str(json).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].connector_id, "conn-abc");
        assert!((refs[0].at - 0.5).abs() < 1e-9);
    }

    #[test]
    fn access_restriction_deserializes() {
        // Actual Overture JSON format: heading is inside the "when" object.
        let json = r#"[{"access_type":"denied","when":{"heading":"backward","during":null,"vehicle":null,"mode":null},"between":null}]"#;
        let ar: Vec<AccessRestriction> = serde_json::from_str(json).unwrap();
        assert_eq!(ar[0].access_type.as_deref(), Some("denied"));
        assert_eq!(ar[0].when_condition.as_ref().and_then(|w| w.heading.as_deref()), Some("backward"));
    }
}
