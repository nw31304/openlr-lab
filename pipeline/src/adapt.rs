use rayon::prelude::*;
use tracing::warn;

use crate::extract::{AccessRestriction, ConnectorRef, OvertureSegment, ProhibitedTransition};
use crate::schema::SchemaMapping;
use openlr_graph::Direction;

pub struct AdaptedSegment {
    pub stable_id: String,
    pub geometry: Vec<(f64, f64)>,
    /// Sorted ascending by `at` position.
    pub connectors: Vec<ConnectorRef>,
    pub frc: u8,
    pub fow: u8,
    pub direction: Direction,
    /// False for classes marked `vehicular = false` in the schema (footways, cycleways, etc.).
    pub vehicular: bool,
    pub prohibited_transitions: Vec<ProhibitedTransition>,
}

pub fn adapt(segments: Vec<OvertureSegment>, schema: &SchemaMapping) -> Vec<AdaptedSegment> {
    segments
        .into_par_iter()
        .map(|seg| adapt_one(seg, schema))
        .collect()
}

fn adapt_one(seg: OvertureSegment, schema: &SchemaMapping) -> AdaptedSegment {
    let (mut frc, mut fow) = schema.lookup(&seg.class, seg.subclass.as_deref());

    let active_flags: Vec<&str> = seg
        .road_flags
        .iter()
        .flat_map(|f| f.values.iter().map(|s| s.as_str()))
        .collect();
    if !active_flags.is_empty() {
        (frc, fow) = schema.apply_flags(frc, fow, &active_flags);
    }

    if frc == 7 && fow == 0 {
        warn!(
            id = %seg.id,
            class = %seg.class,
            subclass = ?seg.subclass,
            "segment matched schema catch-all rule"
        );
    }

    let direction = derive_direction(&seg.access_restrictions);
    let vehicular = schema.is_vehicular(&seg.class, seg.subclass.as_deref());

    let mut connectors = seg.connectors;
    connectors.sort_by(|a, b| {
        a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal)
    });

    AdaptedSegment {
        stable_id: seg.id,
        geometry: seg.geometry,
        connectors,
        frc,
        fow,
        direction,
        vehicular,
        prohibited_transitions: seg.prohibited_transitions,
    }
}

/// Derive travel direction from access restrictions.
///
/// An entry with `access_type = "denied"` and `heading = "backward"` means backward
/// travel is forbidden → segment is FORWARD-only, and vice versa.
/// The `heading` field may appear directly on the restriction or inside `when`.
fn derive_direction(restrictions: &[AccessRestriction]) -> Direction {
    let mut forward_denied = false;
    let mut backward_denied = false;

    for r in restrictions {
        if r.access_type.as_deref() != Some("denied") {
            continue;
        }
        let heading = r
            .heading
            .as_deref()
            .or_else(|| r.when_condition.as_ref()?.heading.as_deref());

        match heading {
            Some("forward")  => forward_denied = true,
            Some("backward") => backward_denied = true,
            _ => {}
        }
    }

    match (forward_denied, backward_denied) {
        (true, false) => Direction::Backward,
        (false, true) => Direction::Forward,
        _             => Direction::Both,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::AccessRestriction;

    fn denied(heading: &str) -> AccessRestriction {
        AccessRestriction {
            access_type: Some("denied".into()),
            when_condition: None,
            heading: Some(heading.into()),
        }
    }

    #[test]
    fn direction_both_when_no_restrictions() {
        assert_eq!(derive_direction(&[]), Direction::Both);
    }

    #[test]
    fn direction_forward_when_backward_denied() {
        assert_eq!(derive_direction(&[denied("backward")]), Direction::Forward);
    }

    #[test]
    fn direction_backward_when_forward_denied() {
        assert_eq!(derive_direction(&[denied("forward")]), Direction::Backward);
    }

    #[test]
    fn direction_both_when_both_denied() {
        // Contradictory restrictions fall back to Both (safe default).
        assert_eq!(
            derive_direction(&[denied("forward"), denied("backward")]),
            Direction::Both
        );
    }

    #[test]
    fn direction_ignores_allowed_entries() {
        let r = AccessRestriction {
            access_type: Some("allowed".into()),
            when_condition: None,
            heading: Some("backward".into()),
        };
        assert_eq!(derive_direction(&[r]), Direction::Both);
    }

    #[test]
    fn direction_heading_from_when_condition() {
        use crate::extract::AccessWhen;
        let r = AccessRestriction {
            access_type: Some("denied".into()),
            when_condition: Some(AccessWhen {
                heading: Some("backward".into()),
                during: None,
                vehicle: None,
                mode: None,
            }),
            heading: None,
        };
        assert_eq!(derive_direction(&[r]), Direction::Forward);
    }
}
