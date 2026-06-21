import React, { useState } from 'react';
import { useStore } from '../store.js';

const SCALAR_FIELDS = [
  { key: 'candidate_search_radius_m',    label: 'Search radius',          unit: 'm',     step: 10,    min: 10,    max: 500    },
  { key: 'snap_to_endpoint_threshold_m', label: 'Snap threshold',         unit: 'm',     step: 1,     min: 0,     max: 50     },
  { key: 'distance_weight',              label: 'Distance weight',         unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'bearing_weight',               label: 'Bearing weight',          unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'bearing_penalty_per_bucket',   label: 'Bearing pen./bucket',     unit: '',      step: 0.01,  min: 0,     max: 1      },
  { key: 'max_bearing_deviation_deg',    label: 'Max bearing deviation',   unit: '°',     step: 5,     min: 0,     max: 180    },
  { key: 'frc_weight',                   label: 'FRC weight',              unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'fow_weight',                   label: 'FOW weight',              unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'interior_weight',              label: 'Interior snap weight',    unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'wrong_endpoint_weight',        label: 'Wrong endpoint weight',   unit: '',      step: 0.05,  min: 0,     max: 2      },
  { key: 'max_candidate_score',          label: 'Max candidate score',     unit: '',      step: 0.05,  min: 0,     max: 5      },
  { key: 'max_candidates_per_lrp',       label: 'Max candidates',          unit: '/LRP',  step: 1,     min: 1,     max: 50,    int: true },
  { key: 'dnp_tolerance_pct',            label: 'DNP tolerance',           unit: '',      step: 0.05,  min: 0,     max: 1      },
  { key: 'max_path_search_factor',       label: 'Path search factor',      unit: '×DNP',  step: 0.5,   min: 1,     max: 20     },
  { key: 'max_astar_expansions',         label: 'A* expansion cap',        unit: '',      step: 10000, min: 0,     max: 500000, int: true },
  { key: 'lfrcnp_tolerance',             label: 'LFRCNP tolerance',        unit: 'steps', step: 1,     min: 0,     max: 7,     int: true },
];

const FRC_LABELS = ['FRC0', 'FRC1', 'FRC2', 'FRC3', 'FRC4', 'FRC5', 'FRC6', 'FRC7'];
const FOW_LABELS = ['Undef', 'Mway', 'MultiC', 'SingleC', 'Roundbt', 'TrafSq', 'Slip', 'Other'];

function decimalsForStep(step) {
  if (step >= 1) return 0;
  return Math.max(0, -Math.floor(Math.log10(step)));
}

function fmt(v, dec, isInt) {
  if (v == null || isNaN(v)) return '0';
  return isInt ? String(Math.round(v)) : String(parseFloat(v.toFixed(dec)));
}

function SpinInput({ value, onChange, step, min, max, isInt, className, style }) {
  const dec = decimalsForStep(step);
  const [text, setText] = React.useState(() => fmt(value, dec, isInt));
  // Ref keeps adjust stale-closure-free across rapid clicks.
  const textRef = React.useRef(text);

  // Sync display when the prop changes from outside (preset applied, etc.).
  React.useEffect(() => {
    const s = fmt(value, dec, isInt);
    textRef.current = s;
    setText(s);
  }, [value]); // eslint-disable-line react-hooks/exhaustive-deps

  const commit = (raw) => {
    const out = isInt
      ? Math.round(Math.min(max, Math.max(min, raw)))
      : parseFloat(Math.min(max, Math.max(min, raw)).toFixed(dec));
    const s = fmt(out, dec, isInt);
    textRef.current = s;
    setText(s);
    onChange(out);
  };

  const adjust = (dir) => {
    const base = parseFloat(textRef.current);
    if (!isNaN(base)) commit(base + dir * step);
  };

  return (
    <div className={`spin-wrap ${className ?? ''}`} style={style}>
      <input
        className="spin-input"
        type="text"
        inputMode="numeric"
        value={text}
        onChange={e => { textRef.current = e.target.value; setText(e.target.value); }}
        onBlur={() => {
          const v = isInt ? parseInt(textRef.current, 10) : parseFloat(textRef.current);
          if (!isNaN(v)) commit(v);
          else { const s = fmt(value, dec, isInt); textRef.current = s; setText(s); }
        }}
        onKeyDown={e => {
          if (e.key === 'Enter') {
            const v = isInt ? parseInt(textRef.current, 10) : parseFloat(textRef.current);
            if (!isNaN(v)) commit(v);
          }
        }}
      />
      <div className="spin-arrows">
        <button type="button" className="spin-arrow" onClick={() => adjust(1)}>▲</button>
        <button type="button" className="spin-arrow" onClick={() => adjust(-1)}>▼</button>
      </div>
    </div>
  );
}

function PenaltyTable({ tableKey, rowLabels, colLabels }) {
  const { params, setTableCell } = useStore();
  const table = params[tableKey];
  if (!table) return null;

  // rows = LRP (reference), cols = segment (map) — mirrors penalty_table[lrp][seg] in engine
  return (
    <div className="penalty-table-wrap">
      <div className="pt-axis-col-label">Segment (map) →</div>
      <div className="pt-axis-row-wrap">
        <div className="pt-axis-row-label">LRP (ref) ↓</div>
        <div className="penalty-table-grid" style={{ gridTemplateColumns: `52px repeat(${colLabels.length}, 52px)` }}>
          <div className="pt-corner" />
          {colLabels.map(l => <div key={l} className="pt-col-label">{l}</div>)}
          {table.map((row, i) => (
            <React.Fragment key={i}>
              <div className="pt-row-label">{rowLabels[i]}</div>
              {row.map((val, j) => (
                <SpinInput
                  key={j}
                  className="pt-spin"
                  value={val}
                  step={0.05}
                  min={0}
                  max={1}
                  isInt={false}
                  onChange={v => setTableCell(tableKey, i, j, v)}
                  style={{ background: `rgba(220,80,80,${val * 0.7})` }}
                />
              ))}
            </React.Fragment>
          ))}
        </div>
      </div>
    </div>
  );
}

export default function ParamsPanel() {
  const { params, showParams, setParam, toggleParams } = useStore();
  const [showAdvanced, setShowAdvanced] = useState(false);
  if (!showParams) return null;

  return (
    <div className="params-panel">
      <div className="params-panel-header">
        <span className="params-panel-title">Decode Parameters</span>
        <button className="params-panel-close" onClick={toggleParams} title="Close">✕</button>
      </div>
      <div className="params-panel-body">
        <div className="params-grid">
          {SCALAR_FIELDS.map(({ key, label, unit, step, min, max, int: isInt }) => (
            <label key={key} className="param-row">
              <span className="param-label">{label}{unit ? <em> {unit}</em> : ''}</span>
              <SpinInput
                value={params[key] ?? 0}
                step={step}
                min={min}
                max={max}
                isInt={isInt}
                onChange={v => setParam(key, v)}
              />
            </label>
          ))}
        </div>

        <button className="advanced-toggle" onClick={() => setShowAdvanced(s => !s)}>
          {showAdvanced ? '▾' : '▸'} Advanced penalty tables
        </button>

        {showAdvanced && (
          <>
            <div className="table-section">
              <div className="table-section-title">FRC penalty table</div>
              <PenaltyTable tableKey="frc_penalty_table" rowLabels={FRC_LABELS} colLabels={FRC_LABELS} />
            </div>

            <div className="table-section">
              <div className="table-section-title">FOW penalty table</div>
              <PenaltyTable tableKey="fow_penalty_table" rowLabels={FOW_LABELS} colLabels={FOW_LABELS} />
            </div>
          </>
        )}
      </div>
    </div>
  );
}
