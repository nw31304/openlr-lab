import React, { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useStore, getSegGeomCache } from '../store.js';
import { buildReplaySteps } from '../replayEngine.js';
import { TOUR_SAMPLE_DECODE_RESULT, TOUR_SAMPLE_OPENLR_STRING, TOUR_SAMPLE_TRACE, tourSampleGeomEntries } from '../tourSampleData.js';

// Cadence for the live A* replay demo (`replayDemo` step) -- one node
// expansion revealed per tick, looping through just the A* portion of the
// sample trace (RouteSearchStarted..RouteFound) rather than the whole decode.
const REPLAY_DEMO_TICK_MS = 700;

// Content only -- which live DOM region(s) each step points at (matched via
// data-tour attributes added in MenuBar.jsx, or existing stable classNames
// for the two side panels), a short title/body, and whether that step needs
// a normally-closed side panel opened first. Union of all matched elements'
// bounding rects becomes the spotlight, so a step can point at a whole
// cluster of buttons (e.g. the four view-toggle buttons) as one region.
const STEPS = [
  {
    target: '[data-tour="mode-toggle"]',
    title: 'Decode vs Encode',
    body: 'Switch between decoding an existing OpenLR reference and drawing a new route to encode one.',
  },
  {
    target: '.bottom-input, .decode-btn',
    title: 'Paste a reference to decode',
    body: 'Paste an OpenLR string here and hit Decode — both binary formats are supported, TomTomV3 and TPEG-OLR, each base64-encoded. The format is detected automatically, no need to specify which.',
    ensure: 'decodeMode',
  },
  {
    target: '[data-tour="view-tabs"]',
    title: 'Views',
    body: 'Segments, Trace, Replay, and Results show different angles on the same decode — toggle whichever ones you want open at once.',
  },
  {
    target: '.side-panel-left',
    title: 'Results panel',
    body: 'The at-a-glance answer: what the reference decoded to, and its constituent road segments. Notice bearing, DNP, and offsets show as ranges (e.g. 191°–203°) — a v3 reference encodes a tolerance bucket, not one exact value, and this tool always keeps both bounds rather than collapsing them to a midpoint. (Sample data shown — nothing has actually been decoded yet.)',
    ensure: 'result',
    showSample: true,
  },
  {
    target: '.side-panel-right',
    title: 'Trace panel',
    body: 'The deep-dive: why the decoder chose these segments — candidates considered, routing, and offsets, every one of them the same [min, max] interval end to end, never averaged down to a single guess. (Same sample decode as the Results panel.)',
    ensure: 'trace',
    showSample: true,
  },
  {
    target: '.tp-forced-bar',
    title: 'Forced re-decode: "what if?"',
    body: 'Pin a specific candidate at any LRP — or click "Pin best candidates" to seed every LRP at once — then re-run A* using exactly those choices instead of what the decoder picked on its own. Great for testing "why didn\'t it use *this* road?" without editing the input string at all.',
  },
  {
    target: '[data-tour-solo="replay-btn"]',
    title: 'Replay: watch it happen',
    body: 'The standout feature — step through every phase of a decode (or an encode\'s verify) one at a time: candidate search, A* routing, offset trimming, all animated live on the map exactly as the engine experienced it. Step forward, back, or scrub the timeline directly.',
  },
  {
    target: '.map-area, .replay-panel',
    title: 'A* routing, live',
    body: 'A live slice of a real decode\'s A* search, playing automatically: the router expands the road graph node by node — g = distance travelled so far, h = straight-line estimate to the target — honoring one-way streets and turn restrictions until it finds the shortest legal path. Replay animates the LRP candidate search the same way (accepted and rejected candidates as colored dots) — this slice just zooms in on the A* portion.',
    ensure: 'replayDemo',
  },
  {
    target: '.params-panel',
    title: 'Decode parameters',
    body: 'A deep, tunable rulebook: FRC/FOW match tolerance, candidate search radius, bearing and DNP windows, LFRCNP tolerance, and more — every knob the decoder uses to pick candidates and validate routes.',
    ensure: 'params',
  },
  {
    target: '[data-tour-solo="tile-source"], [data-tour-solo="tile-source"] .menu-tile-dropdown',
    title: 'Bring your own map',
    body: 'Not locked to one map provider — point this at any PMTiles archive you build or host yourself (TomTom, OSM, Overture, ESRI, whatever you work with) for both decoding and encoding.',
    ensure: 'tileSourceMenu',
  },
  {
    target: '[data-tour="config-tools"]',
    title: 'AI chat and trace detail',
    body: 'Configure an AI provider here to unlock AI Chat — a real diagnostic assistant, not a chatbot guessing from the screen. It calls tools directly into the live engine (candidate scores, A* stats, graph topology) to answer questions grounded in the actual trace, can embed live SVG diagrams, and can even pin candidates and trigger a forced re-decode on your behalf. Trace Level here controls how much detail the *next* decode records for it — and for Replay — to work with.',
  },
  {
    target: '[data-tour="mode-toggle"]',
    title: 'Encoding: a different workflow',
    body: 'Everything so far has been Decode mode: an existing OpenLR string in, a matched route out. Encode mode is a second, self-contained workflow, not a variant of the first — instead of pasting a string, you draw a route directly on the map. The bottom decode input and the Results/Trace/Replay decode buttons disappear entirely, replaced by the encode workflow panel covered next. Switching back and forth never discards either side\'s state — whatever you\'ve decoded and whatever you\'re encoding both wait right where you left them.',
    ensure: 'encodeMode',
  },
  {
    target: '.map-area',
    title: 'Placing waypoints',
    body: 'Right-click empty map to append a waypoint (Line) or place/replace the single point (Point Along Line). Right-click directly on the already-drawn route to insert a via-point mid-leg. Right-click a numbered waypoint marker to move it — a plain left-click on one instead removes it immediately, no confirmation. Any of these can be a right-click-drag: a dashed "ghost" line previews the pending edit live as you drag.',
  },
  {
    target: '.map-area',
    title: 'The snap candidate popup',
    body: 'A waypoint you click isn\'t itself an LRP — it\'s just where the encoder starts looking for a real road to anchor to. Releasing any waypoint edit opens a popup listing nearby roads and intersections to choose from; picking one redraws the actual routed preview for that specific choice, not just a straight line to it, so you can compare before committing rather than silently snapping to the nearest point.',
  },
  {
    target: '.side-panel-left',
    title: 'Encode, then verify the round trip',
    body: 'This panel mirrors Decode\'s Results panel: the waypoint list, a live route preview that updates as you add waypoints (no need to encode first to see what the route looks like), and an Encode button. Once encoded, the result is immediately re-decoded through the exact same engine used everywhere else in this app — a genuine round trip, not a simulated check — reported as a ✓/⚠ verify badge, with the same Trace and Replay available for that verify decode.',
    ensure: 'result',
  },
];

const INTRO_BULLETS = [
  '🗺  Customizable tile sources — bring your own map data',
  '▶  Step-by-step replay of the decode search',
  '✦  AI chat that can answer questions about a decode',
  '↺  Forced re-decode — explore "what if" alternatives',
  '✎  Encode new locations, not just decode existing ones',
];

// A big branded moment before the step-by-step tour: the title morphs
// (FLIP-style transform animation) from its large, centered splash position
// into the real, small menu-bar title's exact position, so it visually
// "becomes" the real UI rather than just cutting away from a static screen.
function IntroSplash({ onStart, onSkip }) {
  const titleRef = useRef(null);
  const [morphing, setMorphing] = useState(false);
  const [morphStyle, setMorphStyle] = useState(null);

  const handleStart = () => {
    const from = titleRef.current?.getBoundingClientRect();
    const to   = document.querySelector('.menu-title')?.getBoundingClientRect();
    if (from && to && from.height > 0) {
      const scale = to.height / from.height;
      const dx = to.left - from.left;
      const dy = to.top - from.top;
      setMorphStyle({ transform: `translate(${dx}px, ${dy}px) scale(${scale})` });
    }
    setMorphing(true);
    setTimeout(onStart, 650);
  };

  return (
    <div className={`tour-intro${morphing ? ' morphing' : ''}`}>
      <div className="tour-intro-backdrop" />
      <div className="tour-intro-glow" />
      <div className="tour-intro-header">
        <div className="tour-intro-title" ref={titleRef} style={morphStyle ?? undefined}>OpenLRLab</div>
        <div className="tour-intro-subtitle"><span>The visual, interactive OpenLR diagnostic toolkit</span></div>
      </div>
      <ul className="tour-intro-bullets">
        {INTRO_BULLETS.map((b, i) => (
          <li key={i} style={{ animationDelay: `${0.1 + i * 0.15}s` }}>{b}</li>
        ))}
      </ul>
      <div className="tour-intro-actions">
        <button className="tour-btn tour-btn-skip" onClick={onSkip} disabled={morphing}>Skip</button>
        <button className="tour-btn tour-btn-primary tour-btn-large" onClick={handleStart} disabled={morphing}>
          Start Tour
        </button>
      </div>
    </div>
  );
}

function unionRect(selector) {
  const els = Array.from(document.querySelectorAll(selector));
  let left = Infinity, top = Infinity, right = -Infinity, bottom = -Infinity;
  for (const el of els) {
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) continue;
    left = Math.min(left, r.left);
    top = Math.min(top, r.top);
    right = Math.max(right, r.right);
    bottom = Math.max(bottom, r.bottom);
  }
  if (left === Infinity) return null;
  return { left, top, right, bottom, width: right - left, height: bottom - top };
}

export default function OnboardingTour() {
  const { tourStep, nextTourStep, prevTourStep, endTour, openResult, openTrace,
          openParams, closeParams, openTileSourceMenu, closeTileSourceMenu, setMode } = useStore();
  const [rect, setRect] = useState(null);
  const rafRef = useRef(null);
  const sampleSnapshotRef   = useRef(null);
  const prevSampleActiveRef = useRef(false);
  const panelSnapshotRef    = useRef(null);
  const prevRunningRef      = useRef(false);

  const running = tourStep != null;
  const active = tourStep != null && tourStep >= 0 && tourStep < STEPS.length;
  const step = active ? STEPS[tourStep] : null;
  // Once the tour reaches a step that wants sample data, keep showing it for
  // the rest of the tour (rather than flickering it on/off step to step) --
  // it turns off only when the tour ends or the user steps back before it.
  const sampleActive = active && STEPS.slice(0, tourStep + 1).some(s => s.showSample);

  // Open whichever side panel this step needs before measuring its rect --
  // both panels default to closed, so pointing at them un-opened would just
  // spotlight a zero-width sliver. Results/Trace are unobtrusive docked side
  // panels, left open for the rest of the tour once shown (no cleanup here).
  // Params (a large floating modal) and the tile-source dropdown are more
  // disruptive if left open once the tour has moved on to a different topic,
  // so those two close again as soon as their own step ends.
  useLayoutEffect(() => {
    if (!step) return;
    if (step.ensure === 'result')      openResult();
    if (step.ensure === 'trace')       openTrace();
    // BottomBar (the paste-a-reference input) only renders in decode mode --
    // force it so this step's target actually exists, regardless of
    // whichever mode was active when the tour was (re)started.
    if (step.ensure === 'decodeMode') setMode('decode');
    // EncodeResultPanel (waypoint list, live preview) only renders in encode
    // mode -- force it so the closing encode-mode steps' targets exist,
    // regardless of whichever mode was active when the tour started. Left in
    // place (no restore) once the tour ends, matching decodeMode's own
    // precedent above.
    if (step.ensure === 'encodeMode') setMode('encode');
    if (step.ensure === 'params') {
      openParams();
      return () => closeParams();
    }
    if (step.ensure === 'tileSourceMenu') {
      openTileSourceMenu();
      return () => closeTileSourceMenu();
    }
    if (step.ensure === 'replayDemo') {
      // Seed the real segment-geometry cache with the sample segments' own
      // geometry (keyed by their real segment_id, matching the trace's own
      // references) so Map.jsx's existing, tile-agnostic replay-visualization
      // pipeline can render this off static data -- snapshot whatever was
      // there first (almost certainly nothing, but a real segment_id
      // collision with the loaded tileset is possible) so it can be restored
      // rather than permanently overwritten.
      const cache = getSegGeomCache();
      const geomEntries = tourSampleGeomEntries();
      const priorGeom = geomEntries.map(([id]) => [id, cache.get(id)]);
      geomEntries.forEach(([id, feat]) => cache.set(id, feat));

      const { steps, stats } = buildReplaySteps(TOUR_SAMPLE_TRACE.events);
      const loSlice = Math.max(0, steps.findIndex(s => s.type === 'route_search_started'));
      const hiFound = steps.findIndex(s => s.type === 'route_found');
      const hiSlice = hiFound >= 0 ? hiFound : steps.length - 1;

      const priorReplay = {
        replaySteps: useStore.getState().replaySteps,
        replayStats: useStore.getState().replayStats,
        replayStep:  useStore.getState().replayStep,
        showReplay:  useStore.getState().showReplay,
      };
      useStore.setState({ replaySteps: steps, replayStats: stats, replayStep: loSlice, showReplay: true });

      let cur = loSlice;
      const timer = setInterval(() => {
        cur = cur >= hiSlice ? loSlice : cur + 1;
        useStore.setState({ replayStep: cur });
      }, REPLAY_DEMO_TICK_MS);

      return () => {
        clearInterval(timer);
        useStore.setState(priorReplay);
        priorGeom.forEach(([id, feat]) => {
          if (feat === undefined) cache.delete(id); else cache.set(id, feat);
        });
      };
    }
  }, [tourStep]); // eslint-disable-line react-hooks/exhaustive-deps

  // Whatever the tour force-opened (Results/Trace panels, Params, tile-source
  // dropdown), close back down to however it was *before* the tour started
  // once it ends -- otherwise it finishes with panels left open (Results/
  // Trace now showing nothing, since the sample data has been swapped back
  // out), which reads as broken/empty.
  useEffect(() => {
    const wasRunning = prevRunningRef.current;
    if (running && !wasRunning) {
      panelSnapshotRef.current = {
        showResult: useStore.getState().showResult,
        showTrace: useStore.getState().showTrace,
        showParams: useStore.getState().showParams,
        showTileSourceMenu: useStore.getState().showTileSourceMenu,
      };
    } else if (!running && wasRunning) {
      const snap = panelSnapshotRef.current;
      if (snap) useStore.setState({
        showResult: snap.showResult,
        showTrace: snap.showTrace,
        showParams: snap.showParams,
        showTileSourceMenu: snap.showTileSourceMenu,
      });
      panelSnapshotRef.current = null;
    }
    prevRunningRef.current = running;
  }, [running]);

  // Swap in a fixed, made-up sample decode result while showing the
  // Results/Trace steps -- not a real decode, so it renders correctly
  // regardless of which tileset/region is actually loaded. Snapshot the
  // real decodeResult/openlrString before swapping, and restore them when
  // sample display ends -- but only if the sample is still in place (if the
  // user ran a real decode mid-tour, that takes precedence and must not be
  // clobbered by restoring the stale pre-tour snapshot).
  useEffect(() => {
    const wasActive = prevSampleActiveRef.current;
    if (sampleActive && !wasActive) {
      sampleSnapshotRef.current = {
        decodeResult: useStore.getState().decodeResult,
        openlrString: useStore.getState().openlrString,
      };
      useStore.setState({
        decodeResult: TOUR_SAMPLE_DECODE_RESULT,
        openlrString: TOUR_SAMPLE_OPENLR_STRING,
      });
    } else if (!sampleActive && wasActive) {
      const snap = sampleSnapshotRef.current;
      if (snap && useStore.getState().decodeResult === TOUR_SAMPLE_DECODE_RESULT) {
        useStore.setState({ decodeResult: snap.decodeResult, openlrString: snap.openlrString });
      }
      sampleSnapshotRef.current = null;
    }
    prevSampleActiveRef.current = sampleActive;
  }, [sampleActive]);

  // Recompute the spotlight rect for ~300ms after a step change (covers the
  // side panels' own 0.2s width transition and any reflow from opening one),
  // then keep it live against window resizes for the rest of the step.
  useEffect(() => {
    if (!step) { setRect(null); return; }
    let frames = 0;
    const tick = () => {
      setRect(unionRect(step.target));
      frames += 1;
      if (frames < 20) rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);

    const onResize = () => setRect(unionRect(step.target));
    window.addEventListener('resize', onResize);
    return () => {
      if (rafRef.current) cancelAnimationFrame(rafRef.current);
      window.removeEventListener('resize', onResize);
    };
  }, [tourStep]); // eslint-disable-line react-hooks/exhaustive-deps

  if (tourStep === -1) return <IntroSplash onStart={nextTourStep} onSkip={endTour} />;

  if (!active || !step) return null;

  const margin = 10;
  const vw = window.innerWidth, vh = window.innerHeight;
  const spot = rect ?? { left: vw / 2 - 1, top: vh / 2 - 1, right: vw / 2 + 1, bottom: vh / 2 + 1, width: 2, height: 2 };

  const tooltipWidth = 300;
  const estTooltipHeight = 150;
  const placeBelow = spot.bottom + margin + estTooltipHeight < vh;
  const tooltipTop = placeBelow ? spot.bottom + margin : Math.max(margin, spot.top - margin - estTooltipHeight);
  const centerX = (spot.left + spot.right) / 2;
  const tooltipLeft = Math.min(Math.max(margin, centerX - tooltipWidth / 2), vw - tooltipWidth - margin);

  return (
    <div className="tour-root">
      {/* Four dimming bands around the spotlight cutout -- the cutout region
          itself has no overlay, so the real UI underneath stays clickable. */}
      <div className="tour-dim" style={{ left: 0, top: 0, width: vw, height: Math.max(0, spot.top) }} />
      <div className="tour-dim" style={{ left: 0, top: spot.bottom, width: vw, height: Math.max(0, vh - spot.bottom) }} />
      <div className="tour-dim" style={{ left: 0, top: spot.top, width: Math.max(0, spot.left), height: spot.height }} />
      <div className="tour-dim" style={{ left: spot.right, top: spot.top, width: Math.max(0, vw - spot.right), height: spot.height }} />

      <div
        className="tour-spotlight-ring"
        style={{ left: spot.left - 4, top: spot.top - 4, width: spot.width + 8, height: spot.height + 8 }}
      />

      <div className="tour-tooltip" style={{ left: tooltipLeft, top: tooltipTop, width: tooltipWidth }}>
        <div className="tour-tooltip-title">{step.title}</div>
        <div className="tour-tooltip-body">{step.body}</div>
        <div className="tour-tooltip-footer">
          <div className="tour-dots">
            {STEPS.map((_, i) => (
              <span key={i} className={`tour-dot${i === tourStep ? ' active' : ''}`} />
            ))}
          </div>
          <div className="tour-tooltip-actions">
            <button className="tour-btn tour-btn-skip" onClick={endTour}>Skip</button>
            {tourStep > 0 && <button className="tour-btn" onClick={prevTourStep}>Back</button>}
            <button className="tour-btn tour-btn-primary" onClick={tourStep === STEPS.length - 1 ? endTour : nextTourStep}>
              {tourStep === STEPS.length - 1 ? 'Done' : 'Next'}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
