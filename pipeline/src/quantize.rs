use rayon::prelude::*;

use crate::split::{NodeRecord, SplitEdge};
use openlr_graph::Direction;

// ── Output types ──────────────────────────────────────────────────────────────

/// A split edge with geometry quantized to the 1e-7 degree grid (~1 cm at equator).
/// `length_cm` is the `length_m` from the split step, scaled and rounded; it is
/// NOT recomputed from the quantized geometry (Invariant 4 — stored length is canonical).
#[derive(Debug, Clone)]
pub struct QuantizedEdge {
    pub start_node_gers: [u8; 16],
    pub end_node_gers: [u8; 16],
    /// (lon_e7, lat_e7) — absolute, not delta-coded (v1).
    pub geometry: Vec<(i32, i32)>,
    /// Length in centimetres (stored as u32; max ~42 km per edge).
    pub length_cm: u32,
    pub frc: u8,
    pub fow: u8,
    pub direction: Direction,
    pub parent_gers_id: [u8; 16],
}

/// A node with quantized coordinates.
#[derive(Debug, Clone)]
pub struct QuantizedNode {
    pub gers_id: [u8; 16],
    pub lon_e7: i32,
    pub lat_e7: i32,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn quantize(
    edges: Vec<SplitEdge>,
    nodes: Vec<NodeRecord>,
) -> (Vec<QuantizedEdge>, Vec<QuantizedNode>) {
    let q_edges: Vec<QuantizedEdge> = edges
        .into_par_iter()
        .map(quantize_edge)
        .collect();

    let q_nodes: Vec<QuantizedNode> = nodes
        .into_par_iter()
        .map(|n| QuantizedNode {
            gers_id: n.gers_id,
            lon_e7: quantize_coord(n.lon),
            lat_e7: quantize_coord(n.lat),
        })
        .collect();

    (q_edges, q_nodes)
}

fn quantize_edge(edge: SplitEdge) -> QuantizedEdge {
    let raw: Vec<(i32, i32)> = edge
        .geometry
        .iter()
        .map(|&(lon, lat)| (quantize_coord(lon), quantize_coord(lat)))
        .collect();

    let geometry = remove_collinear(raw);
    let length_cm = (edge.length_m * 100.0).round() as u32;

    QuantizedEdge {
        start_node_gers: edge.start_node_gers,
        end_node_gers:   edge.end_node_gers,
        geometry,
        length_cm,
        frc:             edge.frc,
        fow:             edge.fow,
        direction:       edge.direction,
        parent_gers_id:  edge.parent_gers_id,
    }
}

// ── Coordinate quantization ───────────────────────────────────────────────────

/// Map a WGS84 degree value to the 1e-7 grid (round-half-to-even).
/// Clamps to i32 range; valid WGS84 values (-180..=180 lon, -90..=90 lat) are
/// well within range, so the clamp only fires on corrupt input.
#[inline]
pub fn quantize_coord(deg: f64) -> i32 {
    (deg * 1e7).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// Restore a quantized coordinate to floating-point degrees.
#[inline]
#[allow(dead_code)]
pub fn dequantize_coord(e7: i32) -> f64 {
    e7 as f64 * 1e-7
}

// ── Lossless collinear-vertex removal ────────────────────────────────────────

/// Remove vertices that are exactly collinear with their neighbors in integer
/// coordinates.  Endpoints are always preserved.  This is the ONLY geometry
/// reduction permitted (Invariant 4).
fn remove_collinear(pts: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    if pts.len() <= 2 {
        return pts;
    }
    let mut out = Vec::with_capacity(pts.len());
    out.push(pts[0]);

    for i in 1..pts.len() - 1 {
        let (x0, y0) = out.last().copied().unwrap();
        let (x1, y1) = pts[i];
        let (x2, y2) = pts[i + 1];
        // 2D integer cross-product; exactly zero ↔ collinear.
        let cross = (x1 - x0) as i64 * (y2 - y0) as i64
                  - (y1 - y0) as i64 * (x2 - x0) as i64;
        if cross != 0 {
            out.push(pts[i]);
        }
    }

    out.push(*pts.last().unwrap());
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_and_back() {
        let lon = 174.765_023_4_f64;
        let q = quantize_coord(lon);
        let back = dequantize_coord(q);
        assert!((back - lon).abs() < 1e-7, "round-trip error: {}", back - lon);
    }

    #[test]
    fn collinear_removal_keeps_endpoints() {
        // All three points collinear on x=0; middle should be dropped.
        let pts = vec![(0i32, 0), (0, 5), (0, 10)];
        let out = remove_collinear(pts);
        assert_eq!(out, vec![(0, 0), (0, 10)]);
    }

    #[test]
    fn collinear_removal_keeps_bend() {
        // Second point is NOT collinear.
        let pts = vec![(0i32, 0), (1, 5), (2, 0)];
        let out = remove_collinear(pts);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn collinear_removal_chain() {
        // p0, p1, p2 collinear; p2, p3 different direction.
        let pts = vec![(0i32, 0), (1, 1), (2, 2), (3, 1)];
        let out = remove_collinear(pts);
        // p1 is collinear between p0 and p2, so dropped.
        assert_eq!(out, vec![(0, 0), (2, 2), (3, 1)]);
    }

    #[test]
    fn two_point_line_unchanged() {
        let pts = vec![(0i32, 0), (1, 1)];
        assert_eq!(remove_collinear(pts.clone()), pts);
    }

    #[test]
    fn length_cm_conversion() {
        let edge = SplitEdge {
            start_node_gers: [0u8; 16],
            end_node_gers:   [0u8; 16],
            geometry:        vec![(174.0, -36.0), (174.001, -36.0)],
            length_m:        111.319,
            frc: 3, fow: 3,
            direction: Direction::Both,
            parent_gers_id: [0u8; 16],
        };
        let qe = quantize_edge(edge);
        assert_eq!(qe.length_cm, 11132); // 111.319 m * 100 = 11131.9 → 11132
    }
}
