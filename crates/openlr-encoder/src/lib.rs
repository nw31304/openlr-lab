//! Turns a concrete path on the road network into an OpenLR location
//! reference. This is the graph-algorithm half of encoding — physical
//! serialization (binary v3 / TPEG-OLR bit-packing) lives in
//! `openlr_codec::encoder`, which this crate's output feeds directly.
//!
//! Only Line and PointAlongLine locations are implemented (see the project's
//! agreed design). The other seven OpenLR location types either need no graph
//! algorithm at all (GeoCoordinate/Circle/Rectangle/Grid/Polygon are pure
//! coordinate validation) or reuse this crate's Line pipeline almost as-is
//! (ClosedLine, PoiWithAccessPoint) — left for later.

pub mod attributes;
pub mod expansion;
pub mod coverage;
pub mod line;
pub mod pal;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("the path has no segments")]
    EmptyPath,
    #[error("path is not connected: segment {index} does not share a node with its predecessor")]
    Disconnected { index: usize },
    #[error("no route exists between the requested points on this graph")]
    NoRoute,
    #[error("tile {0:?} required by the route search is not loaded")]
    NeedsTile(openlr_graph::TileKey),
    #[error("segment {0:?} referenced by the input path is not loaded in this graph")]
    UnknownSegment(openlr_graph::SegmentId),
    #[error("node {0:?} referenced by the input path is not loaded in this graph")]
    UnknownNode(openlr_graph::NodeId),
    #[error(transparent)]
    Codec(#[from] openlr_codec::EncodeError),
}
