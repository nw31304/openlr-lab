use crate::trace::TraceLevel;

/// Pre-tuned parameter presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Preset {
    Permissive,
    Default,
    Strict,
}

// ── Default penalty tables ────────────────────────────────────────────────────

/// FRC mismatch penalties.  Symmetric; row = LRP FRC, col = candidate FRC.
/// Non-linear: small mismatches on important roads are accepted more readily
/// than large mismatches.  0.0 = exact match, 1.0 = maximum penalty.
pub fn default_frc_table() -> [[f64; 8]; 8] {
    // Penalty grows super-linearly with absolute FRC difference.
    const P: [f64; 8] = [0.00, 0.10, 0.25, 0.45, 0.65, 0.80, 0.90, 1.00];
    let mut t = [[0.0f64; 8]; 8];
    for i in 0..8usize {
        for j in 0..8usize {
            t[i][j] = P[i.abs_diff(j)];
        }
    }
    t
}

/// FOW mismatch penalties.  Non-linear / semantic.
/// Rows and columns: 0=Undefined, 1=Motorway, 2=MultipleCarriageway,
/// 3=SingleCarriageway, 4=Roundabout, 5=TrafficSquare, 6=SlipRoad, 7=Other.
///
/// Key cross-map properties encoded in the defaults:
/// - Motorway ↔ MultipleCarriageway: 0.10 (many map sources lack a distinct MC class)
/// - Motorway ↔ SlipRoad:            0.20 (slip roads are attached to motorways)
/// - MultipleCarriageway ↔ Single:   0.20
/// - SingleCarriageway ↔ Roundabout: 0.20 (roundabouts absent on some maps)
/// - Undefined with anything:         0.30 (neutral)
pub fn default_fow_table() -> [[f64; 8]; 8] {
    [
        // row 0: LRP = Undefined
        [0.00, 0.30, 0.30, 0.30, 0.30, 0.30, 0.30, 0.30],
        // row 1: LRP = Motorway
        [0.30, 0.00, 0.10, 0.40, 0.60, 0.70, 0.20, 0.80],
        // row 2: LRP = MultipleCarriageway
        [0.30, 0.10, 0.00, 0.20, 0.40, 0.50, 0.25, 0.70],
        // row 3: LRP = SingleCarriageway
        [0.30, 0.40, 0.20, 0.00, 0.20, 0.25, 0.30, 0.40],
        // row 4: LRP = Roundabout
        [0.30, 0.60, 0.40, 0.20, 0.00, 0.30, 0.40, 0.50],
        // row 5: LRP = TrafficSquare
        [0.30, 0.70, 0.50, 0.25, 0.30, 0.00, 0.50, 0.40],
        // row 6: LRP = SlipRoad
        [0.30, 0.20, 0.25, 0.30, 0.40, 0.50, 0.00, 0.50],
        // row 7: LRP = Other
        [0.30, 0.80, 0.70, 0.40, 0.50, 0.40, 0.50, 0.00],
    ]
}

// ── Parameter struct ──────────────────────────────────────────────────────────

/// Decode-time configuration.  All fields are independently tunable from the UI.
///
/// # Scoring model (lower = better, 0.0 = perfect)
///
/// ```text
/// total = distance_weight        × (distance_m / search_radius_m)
///       + bearing_weight         × (bucket_delta × bearing_penalty_per_bucket)
///       + frc_weight             × frc_penalty_table[lrp_frc][cand_frc]
///       + fow_weight             × fow_penalty_table[lrp_fow][cand_fow]
///       + interior_weight        × (1.0 if interior snap, else 0.0)
///       + wrong_endpoint_weight  × (continuous 0–1 based on position along segment)
/// ```
///
/// Two concepts are intentionally kept separate:
/// - **Penalty value** (table cell, bucket delta) — how large the mismatch is.
/// - **Weight** — how seriously that mismatch should affect candidate ranking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DecodeParams {
    // ── Spatial ──────────────────────────────────────────────────────────────
    /// Maximum distance from an LRP to a candidate segment, meters (hard gate).
    pub candidate_search_radius_m: f64,

    // ── Endpoint snapping ─────────────────────────────────────────────────────
    /// If the projection arc-offset is within this many meters of a segment
    /// endpoint, snap the LRP to that endpoint.
    pub snap_to_endpoint_threshold_m: f64,

    // ── Scoring weights ───────────────────────────────────────────────────────
    /// Weight for the distance component: `distance_m / search_radius_m`.
    pub distance_weight: f64,
    /// Weight for the bearing component.
    pub bearing_weight: f64,
    /// Penalty added to the bearing score per bucket of circular deviation.
    /// Bucket size is taken from the LRP's own bearing interval (`ub − lb`).
    /// Calibrated for v3 (32 sectors of 11.25°); scale down for TPEG (256 sectors).
    pub bearing_penalty_per_bucket: f64,
    /// Weight for the FRC-table component.
    pub frc_weight: f64,
    /// Weight for the FOW-table component.
    pub fow_weight: f64,
    /// Weight for the interior-snap penalty (1.0 when LRP is not at an endpoint).
    pub interior_weight: f64,
    /// Weight for the wrong-endpoint penalty.
    /// Scales continuously from 0.0 (correct end) to 1.0 (wrong end).
    /// For non-last LRPs the "wrong" end is the exit; for the last LRP it is the entry.
    pub wrong_endpoint_weight: f64,

    // ── Penalty tables ────────────────────────────────────────────────────────
    /// 8×8 FRC mismatch table.  `frc_penalty_table[lrp_frc][cand_frc]` ∈ [0, 1].
    pub frc_penalty_table: [[f64; 8]; 8],
    /// 8×8 FOW mismatch table.  `fow_penalty_table[lrp_fow][cand_fow]` ∈ [0, 1].
    pub fow_penalty_table: [[f64; 8]; 8],

    // ── Hard gates ────────────────────────────────────────────────────────────
    /// Maximum bearing excess beyond the encoding interval before a candidate is
    /// rejected outright (degrees).  Set to 180.0 to effectively disable.
    #[serde(default = "default_max_bearing_deviation_deg")]
    pub max_bearing_deviation_deg: f64,
    /// Maximum total candidate score before rejection.  Filters implausible
    /// candidates that passed spatial and bearing gates but are still terrible.
    /// Set to a large value (e.g. 999.0) to effectively disable.
    #[serde(default = "default_max_candidate_score")]
    pub max_candidate_score: f64,

    // ── Candidate set ─────────────────────────────────────────────────────────
    /// Number of top candidates to retain per LRP after scoring (best-first).
    pub max_candidates_per_lrp: usize,

    // ── DNP ───────────────────────────────────────────────────────────────────
    /// DNP tolerance δ as a fraction of expected path length (e.g. 0.25 = 25%).
    pub dnp_tolerance_pct: f64,

    // ── A* ────────────────────────────────────────────────────────────────────
    /// A* expansion cap as a ratio of the expected DNP.
    pub max_path_search_factor: f64,
    /// Hard cap on A* node expansions per leg (0 = unlimited).
    pub max_astar_expansions: usize,
    /// Extra FRC steps added to the encoded LFRCNP floor before passing to A*.
    /// Compensates for FRC mapping differences between encoder and decoder maps.
    pub lfrcnp_tolerance: u8,
    /// Turn-angle gate: an interior A* edge is rejected if the geometric turn at
    /// its node deviates from straight-ahead by more than this many degrees
    /// (`0` = dead ahead only, `180` = a full U-turn). Set to `180.0` to disable.
    /// Independent of `max_bearing_deviation_deg`, which only scores LRP endpoint
    /// candidates and never constrains the interior path search.
    #[serde(default = "default_max_interior_turn_deviation_deg")]
    pub max_interior_turn_deviation_deg: f64,

    // ── Routing ───────────────────────────────────────────────────────────────
    /// Maximum total candidate-combination routing attempts across all legs
    /// (0 = unlimited).  Caps the Kᴺ search space: for N LRPs each with K
    /// candidates the full space is Kᴺ⁻¹; this fires when that bound is hit
    /// before any combination routes successfully.
    #[serde(default = "default_max_routing_attempts")]
    pub max_routing_attempts: usize,

    // ── Trace ─────────────────────────────────────────────────────────────────
    pub trace_level: TraceLevel,
}

fn default_max_bearing_deviation_deg()        -> f64 { 45.0 }
fn default_max_candidate_score()              -> f64 { 1.5 }
fn default_max_routing_attempts()             -> usize { 10 }
fn default_max_interior_turn_deviation_deg()  -> f64 { 150.0 }

impl DecodeParams {
    pub fn preset(p: Preset) -> Self {
        match p {
            Preset::Permissive => Self {
                candidate_search_radius_m:    200.0,
                snap_to_endpoint_threshold_m:  25.0,
                distance_weight:                0.5,
                bearing_weight:                 0.2,
                bearing_penalty_per_bucket:     0.03,
                frc_weight:                     0.05,
                fow_weight:                     0.10,
                interior_weight:                0.05,
                wrong_endpoint_weight:          0.10,
                frc_penalty_table: default_frc_table(),
                fow_penalty_table: default_fow_table(),
                max_bearing_deviation_deg:      90.0,
                max_candidate_score:             1.5,
                max_candidates_per_lrp:          10,
                dnp_tolerance_pct:               0.40,
                max_path_search_factor:          4.0,
                max_astar_expansions:         50_000,
                lfrcnp_tolerance:                  2,
                max_interior_turn_deviation_deg: 180.0,
                max_routing_attempts:              0,
                trace_level: TraceLevel::Summary,
            },
            Preset::Default => Self::default(),
            Preset::Strict => Self {
                candidate_search_radius_m:     50.0,
                snap_to_endpoint_threshold_m:  10.0,
                distance_weight:                0.5,
                bearing_weight:                 0.4,
                bearing_penalty_per_bucket:     0.08,
                frc_weight:                     0.20,
                fow_weight:                     0.30,
                interior_weight:                0.20,
                wrong_endpoint_weight:          0.30,
                frc_penalty_table: default_frc_table(),
                fow_penalty_table: default_fow_table(),
                max_bearing_deviation_deg:      30.0,
                max_candidate_score:             1.0,
                max_candidates_per_lrp:           5,
                dnp_tolerance_pct:               0.10,
                max_path_search_factor:          3.0,
                max_astar_expansions:              0,
                lfrcnp_tolerance:                  0,
                max_interior_turn_deviation_deg: 120.0,
                max_routing_attempts:              5,
                trace_level: TraceLevel::Summary,
            },
        }
    }
}

impl Default for DecodeParams {
    fn default() -> Self {
        Self {
            candidate_search_radius_m:     30.0,
            snap_to_endpoint_threshold_m:  15.0,
            distance_weight:                0.5,
            bearing_weight:                 0.3,
            bearing_penalty_per_bucket:     0.05,
            frc_weight:                     0.10,
            fow_weight:                     0.20,
            interior_weight:                0.10,
            wrong_endpoint_weight:          0.20,
            frc_penalty_table: default_frc_table(),
            fow_penalty_table: default_fow_table(),
            max_bearing_deviation_deg:      45.0,
            max_candidate_score:             1.5,
            max_candidates_per_lrp:           8,
            dnp_tolerance_pct:               0.25,
            max_path_search_factor:          5.0,
            max_astar_expansions:        100_000,
            lfrcnp_tolerance:                  2,
            max_interior_turn_deviation_deg: 150.0,
            max_routing_attempts:             10,
            trace_level: TraceLevel::Summary,
        }
    }
}
