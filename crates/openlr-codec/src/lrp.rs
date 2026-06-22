use crate::{CircularInterval, LinearInterval};

/// A Location Reference Point in the unified interval model.
/// Format-specific bit-unpacking (v3 / TPEG) lives in the decoder modules;
/// everything downstream of the decoder is format-agnostic.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Lrp {
    /// WGS84 (longitude, latitude).
    pub coord: (f64, f64),
    /// Forward bearing from the projection point (or backward for the last LRP).
    pub bearing: CircularInterval,
    pub frc: u8,
    pub fow: u8,
    /// Lowest FRC permitted on the path to the next LRP. None on the last LRP.
    pub lfrcnp: Option<u8>,
    /// Distance to next point in meters. None on the last LRP.
    pub dnp: Option<LinearInterval>,
    /// Positive offset (meters from start of first edge). None if absent.
    pub pos_offset: Option<LinearInterval>,
    /// Negative offset (meters from end of last edge). None if absent.
    pub neg_offset: Option<LinearInterval>,
}

/// Orientation attribute for PointAlongLine — direction of travel at the encoded point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Orientation {
    /// No orientation information / not applicable.
    NoOrientation = 0,
    /// Travel from first LRP toward second LRP.
    FirstTowardSecond = 1,
    /// Travel from second LRP toward first LRP.
    SecondTowardFirst = 2,
    /// Both directions.
    BothDirections = 3,
}

impl Orientation {
    pub fn from_u8(v: u8) -> Self {
        match v & 0x03 {
            0 => Orientation::NoOrientation,
            1 => Orientation::FirstTowardSecond,
            2 => Orientation::SecondTowardFirst,
            _ => Orientation::BothDirections,
        }
    }
}

/// Side-of-road attribute for PointAlongLine — which side of the road the point is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SideOfRoad {
    /// Directly on the road or not applicable.
    DirectlyOnOrNA = 0,
    /// Right side of the road in the direction of travel.
    Right = 1,
    /// Left side of the road in the direction of travel.
    Left = 2,
    /// Both sides.
    Both = 3,
}

impl SideOfRoad {
    pub fn from_u8(v: u8) -> Self {
        match v & 0x03 {
            0 => SideOfRoad::DirectlyOnOrNA,
            1 => SideOfRoad::Right,
            2 => SideOfRoad::Left,
            _ => SideOfRoad::Both,
        }
    }
}

/// Location type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LocationType {
    Line,
    PointAlongLine,
}

impl Default for LocationType {
    fn default() -> Self { LocationType::Line }
}

/// A decoded location reference: an ordered sequence of LRPs with optional PAL attributes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LocationReference {
    pub lrps: Vec<Lrp>,
    #[serde(default)]
    pub location_type: LocationType,
    /// Orientation — set for PointAlongLine, None for Line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<Orientation>,
    /// Side of road — set for PointAlongLine, None for Line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_of_road: Option<SideOfRoad>,
}

impl LocationReference {
    /// Construct a line location reference.
    pub fn line(lrps: Vec<Lrp>) -> Self {
        LocationReference {
            lrps,
            location_type: LocationType::Line,
            orientation: None,
            side_of_road: None,
        }
    }

    /// Construct a PointAlongLine location reference.
    pub fn point_along_line(lrps: Vec<Lrp>, orientation: Orientation, side_of_road: SideOfRoad) -> Self {
        LocationReference {
            lrps,
            location_type: LocationType::PointAlongLine,
            orientation: Some(orientation),
            side_of_road: Some(side_of_road),
        }
    }

    pub fn is_point(&self) -> bool {
        self.location_type == LocationType::PointAlongLine
    }
}
