// ── Haversine ─────────────────────────────────────────────────────────────────

function haversineM(lat1, lon1, lat2, lon2) {
  const R  = 6_371_000;
  const φ1 = lat1 * Math.PI / 180, φ2 = lat2 * Math.PI / 180;
  const Δφ = (lat2 - lat1) * Math.PI / 180;
  const Δλ = (lon2 - lon1) * Math.PI / 180;
  const a  = Math.sin(Δφ / 2) ** 2 + Math.cos(φ1) * Math.cos(φ2) * Math.sin(Δλ / 2) ** 2;
  return R * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
}

// ── Success warnings ──────────────────────────────────────────────────────────

/**
 * Detect degenerate successful decodes — cases where the engine reported
 * success but the result is almost certainly meaningless.
 *
 * Returns { headline, bullets: string[], suggestions: string[] } or null.
 */
export function diagnoseSuccess(result) {
  const lrps   = result?.lrps ?? [];
  const events = result?.trace?.events ?? [];
  const ofType = (key) => events.filter(e => e[key] !== undefined).map(e => e[key]);
  const dnpChecked = ofType('DnpChecked');

  const bullets     = [];
  const suggestions = [];

  // Signal 1: route length near zero on any leg (both LRP anchors snapped to
  // the same node; the engine found a trivial zero-length path).
  for (const d of dnpChecked) {
    if (d.actual_m != null && d.actual_m < 10) {
      bullets.push(
        `Leg ${d.leg}: routed path is only ${d.actual_m.toFixed(1)} m — ` +
        `both LRP anchors appear to have snapped to the same map location.`
      );
    }
  }

  // Signal 2: adjacent LRP encoded coordinates are very close.
  for (let i = 0; i < lrps.length - 1; i++) {
    const a = lrps[i], b = lrps[i + 1];
    if (!a || !b) continue;
    const dist = haversineM(a.lat, a.lon, b.lat, b.lon);
    if (dist < 25) {
      bullets.push(
        `LRP ${i} and LRP ${i + 1} are only ${dist.toFixed(1)} m apart in the ` +
        `encoded reference — likely snapped to the same map node.`
      );
    }
  }

  if (bullets.length === 0) return null;

  suggestions.push(
    'Check whether the encoding map has a short connector segment at this location that is absent from the decoding map.'
  );
  suggestions.push(
    'When both LRP anchors snap to the same point the decoded location covers no meaningful path, even though the decode reports success.'
  );

  return {
    headline: 'Decode succeeded but result may be degenerate',
    bullets,
    suggestions,
  };
}

// ── Failure diagnosis ─────────────────────────────────────────────────────────

/**
 * Synthesise a human-readable failure diagnosis from decode trace events.
 *
 * Returns { headline, bullets: string[], suggestions: string[] } or null if
 * there is nothing to add beyond the raw error string.
 */
export function diagnoseFailure(result) {
  const events = result?.trace?.events;
  if (!events?.length) return null;

  // Index events by their serde externally-tagged key.
  const ofType = (key) => events.filter(e => e[key] !== undefined).map(e => e[key]);

  const ranked      = ofType('CandidatesRanked');
  const terminated  = ofType('AStarTerminated');
  const routeFailed = ofType('RouteFailed');
  const complete    = events.find(e => e.DecodeComplete)?.DecodeComplete;

  if (!complete) return null;

  if (complete.NoCandidates !== undefined) {
    return diagnoseNoCandidates(complete.NoCandidates.lrp_idx, ranked);
  }
  if (complete.NoRoute !== undefined) {
    return diagnoseNoRoute(complete.NoRoute.leg, ranked, terminated, routeFailed);
  }
  return null;
}

// ── NoCandidates ─────────────────────────────────────────────────────────────

function diagnoseNoCandidates(lrpIdx, ranked) {
  const ev = ranked.find(r => r.lrp_idx === lrpIdx);
  if (!ev) return { headline: `No candidates found for LRP ${lrpIdx}`, bullets: [], suggestions: [] };

  const sf = ev.segments_fetched ?? 0;
  if (sf === 0) {
    return {
      headline: `Coverage gap at LRP ${lrpIdx}`,
      bullets: [
        `No road segments exist in the map within the search radius.`,
        `This tile region may not have been built yet, or the LRP coordinate is in an area with no mapped roads.`,
      ],
      suggestions: [
        'Verify the LRP coordinate is in a mapped area.',
        'If using a regional tile build, ensure the area is covered.',
      ],
    };
  }

  const rej = ev.rejected ?? [];
  const accepted = (ev.accepted ?? []).length;
  if (accepted > 0) {
    // Shouldn't reach NoCandidates if accepted > 0, but just in case.
    return null;
  }

  const breakdown = rejectionBreakdown(rej);
  const bullets = [
    `${sf} road segment${sf !== 1 ? 's' : ''} found within search radius, but none passed candidate filters.`,
    ...breakdown.map(([label, count]) => `${count} rejected for ${label}.`),
  ];

  const suggestions = [];
  const hasRadius  = rej.some(r => r.verdict?.FailRadius);
  const hasBearing = rej.some(r => r.verdict?.FailBearing);
  const hasScore   = rej.some(r => r.verdict?.FailScore);

  if (hasRadius)  suggestions.push('Increase candidate search radius.');
  if (hasBearing) suggestions.push('Increase bearing tolerance (max bearing deviation).');
  if (hasScore)   suggestions.push('Increase max candidate score threshold.');

  return { headline: `No valid candidates at LRP ${lrpIdx}`, bullets, suggestions };
}

// ── NoRoute ───────────────────────────────────────────────────────────────────

function diagnoseNoRoute(failedLeg, ranked, terminated, routeFailed) {
  // Aggregate termination data across all A* runs (there may be multiple candidate pairs).
  const legTerminated = terminated.filter(t => t.leg === failedLeg);
  const legFailed     = routeFailed.filter(f => f.leg === failedLeg);

  // Check for DNP mismatch — all failures are DnpOutOfRange?
  const dnpFailures = legFailed.filter(f => f.reason?.DnpOutOfRange !== undefined);
  if (dnpFailures.length > 0 && dnpFailures.length === legFailed.length) {
    const { actual_m, window } = dnpFailures[0].reason.DnpOutOfRange;
    const lb = window?.lb ?? 0, ub = window?.ub ?? 0;
    const over  = actual_m > ub ? (actual_m - ub).toFixed(0) : null;
    const under = actual_m < lb ? (lb - actual_m).toFixed(0) : null;
    return {
      headline: `Route found but DNP out of range on leg ${failedLeg}`,
      bullets: [
        `Best path length: ${actual_m.toFixed(0)} m`,
        `Expected range: [${lb.toFixed(0)}, ${ub.toFixed(0)}] m`,
        over  ? `Path is ${over} m too long.`  : null,
        under ? `Path is ${under} m too short.` : null,
      ].filter(Boolean),
      suggestions: [
        'Increase DNP tolerance (dnp_tolerance_pct) to widen the acceptance window.',
        'Check whether the encoded reference has an accurate distance-to-next-point value.',
      ],
    };
  }

  if (legTerminated.length === 0) {
    return {
      headline: `No route found for leg ${failedLeg}`,
      bullets: ['No path connected the candidate LRPs within the search constraints.'],
      suggestions: ['Try increasing max path search factor or candidate search radius.'],
    };
  }

  // Aggregate skip counts across all terminations for this leg.
  let totalExpanded = 0, totalFrc = 0, totalDir = 0, totalTurn = 0, totalDist = 0;
  let hitLimit = false;
  let expansionLimit = 0;
  for (const t of legTerminated) {
    totalExpanded += t.nodes_expanded ?? 0;
    totalFrc      += t.edges_skipped_frc       ?? 0;
    totalDir      += t.edges_skipped_direction ?? 0;
    totalTurn     += t.edges_skipped_turn      ?? 0;
    totalDist     += t.edges_skipped_distance  ?? 0;
    if (t.reason?.ExpansionLimitHit !== undefined) {
      hitLimit = true;
      expansionLimit = t.reason.ExpansionLimitHit.limit;
    }
  }
  const totalSkipped = totalFrc + totalDir + totalTurn + totalDist;

  const bullets = [];
  const suggestions = [];

  if (hitLimit) {
    bullets.push(`A* hit the expansion limit (${expansionLimit.toLocaleString()} nodes) before finding a path.`);
    suggestions.push('Increase max A* expansions (max_astar_expansions).');
  } else {
    bullets.push(`A* exhausted the search space (${totalExpanded.toLocaleString()} node${totalExpanded !== 1 ? 's' : ''} expanded) without finding a path.`);
  }

  if (totalSkipped > 0) {
    if (totalFrc > 0) {
      bullets.push(`${totalFrc} edge${totalFrc !== 1 ? 's' : ''} skipped due to FRC constraint (LFRCNP floor).`);
      suggestions.push('Lower the LFRCNP floor: the reference may use lower-class roads than the encoded LFRCNP allows.');
    }
    if (totalTurn > 0) {
      bullets.push(`${totalTurn} edge${totalTurn !== 1 ? 's' : ''} blocked by turn restrictions.`);
    }
    if (totalDir > 0) {
      bullets.push(`${totalDir} edge${totalDir !== 1 ? 's' : ''} blocked by one-way direction.`);
    }
    if (totalDist > 0) {
      bullets.push(`${totalDist} edge${totalDist !== 1 ? 's' : ''} pruned for exceeding max search distance.`);
      if (!hitLimit) suggestions.push('Increase max path search factor to allow longer detours.');
    }
  }

  // Check for NoCandidates on any LRP — if all ranked events for any LRP have 0 accepted,
  // the leg has no starts/goals to route between.
  const emptyLrps = ranked.filter(r => (r.accepted ?? []).length === 0);
  if (emptyLrps.length > 0) {
    const idxs = [...new Set(emptyLrps.map(r => r.lrp_idx))];
    bullets.push(`LRP${idxs.length > 1 ? 's' : ''} ${idxs.join(', ')} produced no accepted candidates — the candidate combination search had nothing to route between.`);
  }

  return {
    headline: `No route found for leg ${failedLeg}`,
    bullets,
    suggestions: [...new Set(suggestions)],
  };
}

// ── Segment coverage diagnosis ────────────────────────────────────────────────

/**
 * Explain why a specific map segment was not included in the decoded path.
 *
 * segId:      WASM segment_id integer, or null if the segment was not loaded
 *             during the decode.
 * segProps:   GeoJSON feature properties { frc, fow, direction, tile,
 *             local_index, length_m }
 * decodeResult: the full decode result object from the store.
 *
 * Returns { headline, bullets: string[], suggestions: string[] }.
 */
export function diagnoseSegment(segId, segProps, decodeResult, lfrcnpTolerance = 0) {
  const lrps     = decodeResult?.lrps ?? [];
  const segments = decodeResult?.segments ?? [];
  const events   = decodeResult?.trace?.events ?? [];
  const ofType   = (key) => events.filter(e => e[key] !== undefined).map(e => e[key]);

  // Already in the decoded path?
  const inPath = segments.some(
    s => s.tile === segProps.tile && s.local_index === segProps.local_index
  );
  if (inPath) {
    return {
      headline: 'This segment is part of the decoded path.',
      bullets: [],
      suggestions: [],
    };
  }

  const ranked  = ofType('CandidatesRanked');
  const bullets = [];
  const suggestions = [];

  // ── Candidate search analysis ───────────────────────────────────────────────
  const candidacies = [];
  if (segId != null && ranked.length > 0) {
    for (const ev of ranked) {
      const accepted = ev.accepted?.find(c => c.segment_id === segId);
      const rejected = ev.rejected?.find(c => c.segment_id === segId);
      if (accepted || rejected) {
        candidacies.push({ lrp_idx: ev.lrp_idx, accepted, rejected });
      }
    }
  }

  if (segId == null) {
    bullets.push('Segment ID unavailable — run a decode first to enable full analysis.');
  } else if (ranked.length === 0) {
    bullets.push('No trace data. Re-decode with tracing enabled for a detailed explanation.');
    suggestions.push('Use "Re-decode with tracing" in the result panel, then analyse again.');
  } else if (candidacies.length === 0) {
    bullets.push('This segment was not fetched during candidate search for any LRP.');
    bullets.push('It lies outside the candidate search radius of all encoded LRPs.');
    suggestions.push('Increase candidate search radius to capture more distant segments.');
  } else {
    for (const { lrp_idx, accepted, rejected } of candidacies) {
      if (rejected) {
        const reason = segVerdictReason(rejected.verdict);
        bullets.push(`LRP ${lrp_idx}: rejected as candidate — ${reason ?? 'no specific reason recorded'}.`);
        if (rejected.verdict?.FailBearing) {
          suggestions.push('Increase max bearing deviation to allow this segment as a candidate.');
        } else if (rejected.verdict?.FailScore) {
          suggestions.push('Increase max candidate score threshold.');
        } else if (rejected.verdict?.FailRadius) {
          suggestions.push('Increase candidate search radius.');
        }
      } else if (accepted) {
        const total = accepted.score?.total;
        const scoreStr = total != null ? ` (score ${total.toFixed(4)})` : '';
        bullets.push(
          `LRP ${lrp_idx}: accepted as candidate${scoreStr} — a competing path scored lower and was chosen instead.`
        );
        if (accepted.score) {
          const s = accepted.score;
          const dominant = [
            s.bearing_score        > 0.05 ? `bearing ${s.bearing_score.toFixed(3)}`       : null,
            s.frc_score            > 0.05 ? `FRC ${s.frc_score.toFixed(3)}`               : null,
            s.fow_score            > 0.05 ? `FOW ${s.fow_score.toFixed(3)}`               : null,
            s.distance_score       > 0.05 ? `distance ${s.distance_score.toFixed(3)}`     : null,
            s.wrong_endpoint_score > 0.05 ? `wrong-EP ${s.wrong_endpoint_score.toFixed(3)}` : null,
            s.interior_score       > 0.05 ? `interior ${s.interior_score.toFixed(3)}`     : null,
          ].filter(Boolean);
          if (dominant.length > 0) {
            bullets.push(`Penalty contributors: ${dominant.join(', ')}.`);
          }
        }
        suggestions.push('Adjust scoring weights (FRC, FOW, bearing) to favour this segment\'s attributes.');
      }
    }
  }

  // ── Static FRC routing constraint ──────────────────────────────────────────
  // Effective LFRCNP floor = min(encoded_lfrcnp + lfrcnp_tolerance, 7), matching the engine.
  const segFrc = segProps.frc != null ? parseInt(segProps.frc, 10) : null;
  if (segFrc != null) {
    const frcBlocked = [];
    for (let i = 0; i < lrps.length - 1; i++) {
      const encoded = lrps[i].lfrcnp;
      if (encoded == null) continue;
      const effective = Math.min(encoded + lfrcnpTolerance, 7);
      if (segFrc > effective) {
        const tolNote = lfrcnpTolerance > 0 ? ` encoded ${encoded} + tolerance ${lfrcnpTolerance}` : '';
        frcBlocked.push(`leg ${i} (floor ${effective}${tolNote})`);
      }
    }
    if (frcBlocked.length > 0) {
      bullets.push(`FRC ${segFrc} exceeds the effective LFRCNP routing floor for ${frcBlocked.join(', ')} — A* cannot route through this segment on those legs.`);
      suggestions.push('Increase the LFRCNP tolerance parameter, or the encoded LFRCNP may be too restrictive for this road class.');
    }
  }

  // ── Direction note ──────────────────────────────────────────────────────────
  const dir = segProps.direction;
  if (dir && dir !== 'Both' && dir !== 'BOTH') {
    bullets.push(`One-way segment (${dir}) — only valid in one traversal direction.`);
  }

  // ── Full trace: AStarEdgeSkipped ────────────────────────────────────────────
  const edgeSkipped = ofType('AStarEdgeSkipped');
  if (segId != null && edgeSkipped.length > 0) {
    const skips = edgeSkipped.filter(e => e.segment_id === segId);
    if (skips.length > 0) {
      const reasons = [...new Set(skips.map(e => e.reason ?? 'unknown'))];
      bullets.push(
        `A* explicitly skipped this segment ${skips.length} time${skips.length > 1 ? 's' : ''} during routing: ${reasons.join(', ')}.`
      );
    }
  }

  if (bullets.length === 0) {
    bullets.push('No trace data available for detailed analysis.');
    if (!decodeResult?.trace) {
      suggestions.push('Re-decode with tracing enabled for more detail.');
    }
  }

  const headline = candidacies.some(c => c.accepted)  ? 'Candidate evaluated — not selected on best path'
    : candidacies.some(c => c.rejected) ? 'Candidate rejected during LRP matching'
    : segId != null && ranked.length > 0 ? 'Outside candidate search area for all LRPs'
    : 'Segment not evaluated';

  return { headline, bullets, suggestions: [...new Set(suggestions)] };
}

function segVerdictReason(verdict) {
  if (!verdict || verdict === 'Pass') return null;
  if (verdict === 'FailDirection') return 'degenerate geometry';
  if (verdict.FailRadius)  return `outside search radius (${(verdict.FailRadius.distance_m ?? 0).toFixed(0)} m from LRP)`;
  if (verdict.FailBearing) return `bearing gate — ${(verdict.FailBearing.excess_deg ?? 0).toFixed(1)}° outside the tolerance`;
  if (verdict.FailScore)   return `candidate score too high (${(verdict.FailScore.total ?? 0).toFixed(4)})`;
  return String(verdict);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function rejectionBreakdown(rejected) {
  const counts = {};
  for (const r of rejected) {
    const label = verdictLabel(r.verdict) ?? 'other reason';
    const key = label.replace(/\s*\(.*\)$/, '');
    counts[key] = (counts[key] ?? 0) + 1;
  }
  return Object.entries(counts).sort((a, b) => b[1] - a[1]);
}

function verdictLabel(verdict) {
  if (!verdict || verdict === 'Pass') return null;
  if (verdict === 'FailDirection') return 'degenerate geometry';
  if (verdict.FailRadius)  return `distance > search radius (${verdict.FailRadius.distance_m.toFixed(0)} m)`;
  if (verdict.FailBearing) return `bearing mismatch (${verdict.FailBearing.excess_deg.toFixed(1)}° over limit)`;
  if (verdict.FailScore)   return `total score too high (${verdict.FailScore.total.toFixed(2)})`;
  return 'unknown reason';
}
