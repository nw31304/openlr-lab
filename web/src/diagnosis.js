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
