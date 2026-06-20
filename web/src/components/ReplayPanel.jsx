import React, { useEffect } from 'react';
import { useStore } from '../store.js';

export default function ReplayPanel() {
  const replaySteps   = useStore(s => s.replaySteps);
  const replayStats   = useStore(s => s.replayStats);
  const replayStep    = useStore(s => s.replayStep);
  const stepReplay    = useStore(s => s.stepReplay);
  const setReplayStep = useStore(s => s.setReplayStep);

  const total = replaySteps.length;

  // Keyboard: left/right arrows step the replay
  useEffect(() => {
    const handler = (e) => {
      if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;
      if (e.key === 'ArrowRight') { e.preventDefault(); stepReplay(1); }
      if (e.key === 'ArrowLeft')  { e.preventDefault(); stepReplay(-1); }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [stepReplay]);

  if (total === 0) {
    return (
      <div className="replay-panel replay-panel-empty">
        <span className="replay-hint">
          No replay data — decode with trace level <strong>Summary</strong> or <strong>Full</strong>
        </span>
      </div>
    );
  }

  const currentStep = replaySteps[replayStep];
  const pct         = total > 1 ? (replayStep / (total - 1)) * 100 : 0;
  const phases      = replayStats?.phases ?? [];
  const noAstar     = replayStats?.totalNodes === 0;

  return (
    <div className="replay-panel">
      <div className="replay-controls">
        <span className="rp-label">Replay</span>
        <button className="rp-btn" title="Step back (←)" onClick={() => stepReplay(-1)} disabled={replayStep <= 0}>◀</button>
        <button className="rp-btn" title="Step forward (→)" onClick={() => stepReplay(1)} disabled={replayStep >= total - 1}>▶</button>

        <span className="replay-counter">
          <span className="rp-step-num">{replayStep + 1}</span>
          <span className="rp-step-sep">/</span>
          <span className="rp-step-tot">{total}</span>
          {replayStats?.totalNodes > 0 && (
            <span className="rp-astar-count">· {replayStats.totalNodes.toLocaleString()} A* nodes</span>
          )}
        </span>
      </div>

      <div className="replay-status">{describeStep(currentStep)}</div>

      {noAstar && (
        <div className="rp-hint-bar">
          ⚙ Set <strong>Trace level → Full</strong> and decode again to see A* node expansion
        </div>
      )}

      <TimelineBar pct={pct} phases={phases} total={total} onScrub={setReplayStep} />
    </div>
  );
}

function TimelineBar({ pct, phases, total, onScrub }) {
  function scrubAt(clientX, rect) {
    const p = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    onScrub(Math.round(p * (total - 1)));
  }

  function onMouseDown(e) {
    e.preventDefault();
    const rect = e.currentTarget.getBoundingClientRect();
    scrubAt(e.clientX, rect);
    const onMove = (me) => scrubAt(me.clientX, rect);
    const onUp   = () => { document.removeEventListener('mousemove', onMove); document.removeEventListener('mouseup', onUp); };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup',  onUp);
  }

  return (
    <div className="replay-timeline-wrap">
      <div className="replay-timeline" onMouseDown={onMouseDown}>
        {phases.map((ph, i) => {
          const next  = phases[i + 1];
          const start = ph.startStep / Math.max(1, total - 1) * 100;
          const end   = next ? next.startStep / Math.max(1, total - 1) * 100 : 100;
          return (
            <div key={i} className="rp-phase-strip"
              style={{ left: `${start}%`, width: `${end - start}%`, background: ph.color + '33', borderLeft: `2px solid ${ph.color}77` }}
              title={ph.label}
            />
          );
        })}
        <div className="rp-progress" style={{ width: `${pct}%` }} />
        <div className="rp-handle"   style={{ left:  `${pct}%` }} />
      </div>
      <div className="rp-phase-labels">
        {phases.map((ph, i) => {
          const pos = ph.startStep / Math.max(1, total - 1) * 100;
          return (
            <span key={i} className="rp-phase-label" style={{ left: `${pos}%`, color: ph.color }}>{ph.label}</span>
          );
        })}
      </div>
    </div>
  );
}

// ── Rejection reason helpers ──────────────────────────────────────────────────

function verdictLabel(verdict) {
  if (!verdict || verdict === 'Pass') return null;
  if (verdict === 'FailDirection') return 'degenerate geometry';
  if (verdict.FailRadius)  return `too far (${verdict.FailRadius.distance_m.toFixed(0)} m)`;
  if (verdict.FailBearing) return `bearing off ${verdict.FailBearing.excess_deg.toFixed(1)}° (max ${verdict.FailBearing.max_deg.toFixed(1)}°)`;
  if (verdict.FailScore)   return `score too high (${verdict.FailScore.total.toFixed(2)})`;
  return 'rejected';
}

function rejectionBreakdown(rejected) {
  const counts = {};
  for (const r of rejected) {
    const label = verdictLabel(r.verdict) ?? 'other';
    const key = label.replace(/\s*\(.*\)$/, ''); // strip numeric detail for grouping
    counts[key] = (counts[key] ?? 0) + 1;
  }
  return Object.entries(counts).map(([k, n]) => `${n} × ${k}`).join(', ');
}

// ── Step description ──────────────────────────────────────────────────────────

function describeStep(step) {
  if (!step) return '—';
  switch (step.type) {
    case 'search_started':
      return `LRP ${step.lrp_idx} — candidate search · radius ${step.radius_m.toFixed(0)} m`;

    case 'candidates_ranked': {
      const a  = (step.accepted ?? []).length;
      const rej = step.rejected ?? [];
      const sf  = step.segments_fetched ?? 0;
      if (sf === 0) return `LRP ${step.lrp_idx} — no road segments in search area (coverage gap)`;
      if (a === 0) {
        const breakdown = rejectionBreakdown(rej);
        return `LRP ${step.lrp_idx} — 0 accepted · ${rej.length} rejected from ${sf} segments${breakdown ? ` (${breakdown})` : ''}`;
      }
      return `LRP ${step.lrp_idx} — ${a} accepted · ${rej.length} rejected from ${sf} segments`;
    }

    case 'route_search_started':
      return `Leg ${step.leg} — A* route search started`;

    case 'astar_batch': {
      const n = step.nodes[0];
      return `Leg ${step.leg} — A* node · g=${n.g_m.toFixed(0)} m · h=${n.h_m.toFixed(0)} m`;
    }

    case 'astar_terminated': {
      const reason = step.reason;
      const frc    = step.edges_skipped_frc       ?? 0;
      const dir    = step.edges_skipped_direction  ?? 0;
      const turn   = step.edges_skipped_turn       ?? 0;
      const dist   = step.edges_skipped_distance   ?? 0;
      const parts  = [];
      if (frc  > 0) parts.push(`${frc} FRC-blocked`);
      if (turn > 0) parts.push(`${turn} turn-restricted`);
      if (dir  > 0) parts.push(`${dir} direction-blocked`);
      if (dist > 0) parts.push(`${dist} over-distance`);
      const skipStr = parts.length > 0 ? ` · skips: ${parts.join(', ')}` : '';
      if (reason === 'OpenSetExhausted' || reason?.OpenSetExhausted !== undefined) {
        return `Leg ${step.leg} — A* exhausted · ${step.nodes_expanded} nodes expanded${skipStr}`;
      }
      const limit = reason?.ExpansionLimitHit?.limit ?? 0;
      return `Leg ${step.leg} — A* limit hit (${step.nodes_expanded}/${limit})${skipStr} · raise max_astar_expansions`;
    }

    case 'route_found':
      return `Leg ${step.leg} — route found · ${step.length_m.toFixed(0)} m · ${step.path.length} seg${step.path.length !== 1 ? 's' : ''}`;

    case 'route_failed': {
      const reason = step.reason;
      if (!reason || reason === 'NoPathFound' || reason?.NoPathFound !== undefined) {
        return `Leg ${step.leg} — route FAILED (no path found)`;
      }
      if (reason.DnpOutOfRange) {
        const { actual_m, window } = reason.DnpOutOfRange;
        const lb = window?.lb ?? 0, ub = window?.ub ?? 0;
        const over  = actual_m > ub ? `${(actual_m - ub).toFixed(0)} m over` : null;
        const under = actual_m < lb ? `${(lb - actual_m).toFixed(0)} m under` : null;
        return `Leg ${step.leg} — DNP mismatch · ${actual_m.toFixed(0)} m vs [${lb.toFixed(0)}, ${ub.toFixed(0)}] m (${over ?? under ?? 'failed'})`;
      }
      return `Leg ${step.leg} — route FAILED`;
    }

    case 'dnp_checked': {
      const lb = step.interval?.lb ?? 0, ub = step.interval?.ub ?? 0;
      return `Leg ${step.leg} — DNP ${step.actual_m.toFixed(0)} m ∈ [${lb.toFixed(0)}, ${ub.toFixed(0)}] ${step.passed ? '✓' : '✗'}`;
    }

    case 'offset_applied':
      return `${step.is_positive ? 'Positive' : 'Negative'} offset · trim ${step.trim_m.toFixed(0)} m`;

    case 'decode_complete': {
      const o = step.outcome;
      if (o.Success)       return `✓ Complete · ${o.Success.path.length} segments`;
      if (o.NoCandidates)  return `✗ No candidates for LRP ${o.NoCandidates.lrp_idx}`;
      if (o.NoRoute)       return `✗ No route for leg ${o.NoRoute.leg}`;
      return '✗ Decode failed';
    }

    default: return step.type;
  }
}
