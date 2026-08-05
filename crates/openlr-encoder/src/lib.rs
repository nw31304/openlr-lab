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
pub mod diagnose;
pub mod line;
pub mod pal;
pub mod virtual_split;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("the path has no segments")]
    EmptyPath,
    #[error("path is not connected: segment {index} does not share a node with its predecessor")]
    Disconnected { index: usize },
    /// Distinct from `Disconnected`: segment `segment` at path index `index`
    /// *does* touch the node the walk arrived at, but can't actually be
    /// departed from there — a one-way segment oriented the wrong way for
    /// the direction this path requires. Unlike `Disconnected`, this can
    /// only be caused by direction, never by topology alone — see
    /// `Graph::outgoing_segments`'s doc comment and CLAUDE.md Invariant 10.
    #[error("segment {segment:?} at path index {index} cannot be departed in the required direction")]
    IllegalDirection { index: usize, segment: openlr_graph::SegmentId },
    #[error("no route exists between the requested points on this graph")]
    NoRoute,
    /// A leg over `max_leg_m` is normally handled automatically —
    /// `virtual_split::split_for_rule1` inserts intermediate LRPs (preferring
    /// a valid node, then any node, then a virtual mid-segment point) so
    /// every consecutive-LRP distance stays under the cap. This variant now
    /// only ever surfaces for a degenerate `max_leg_m` itself (`<= 0` or
    /// non-finite) — a real configuration error, not a "this route can't be
    /// encoded" signal, since the splitter can always make progress for any
    /// positive finite cap.
    #[error("leg length {length_m:.0}m exceeds the {max_leg_m:.0}m Rule-1 cap")]
    LegTooLong { length_m: f64, max_leg_m: f64 },
    #[error("tile {0:?} required by the route search is not loaded")]
    NeedsTile(openlr_graph::TileKey),
    #[error("segment {0:?} referenced by the input path is not loaded in this graph")]
    UnknownSegment(openlr_graph::SegmentId),
    #[error("node {0:?} referenced by the input path is not loaded in this graph")]
    UnknownNode(openlr_graph::NodeId),
    #[error(transparent)]
    Codec(#[from] openlr_codec::EncodeError),
}
