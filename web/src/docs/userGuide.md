# OpenLRLab User Guide

A browser-based, visual diagnostic tool for decoding and encoding [OpenLR](https://www.openlr.org/)
location references — both **TomTomV3** (binary) and **TPEG-OLR** (ISO 21219-22) formats. Everything
runs client-side in WebAssembly; the only network activity is fetching map tiles from whichever
[PMTiles](https://protomaps.com/) archive is configured.

This guide covers both **decoding** (an existing OpenLR string → a map location) and **encoding** (a
map location you draw → a new OpenLR string) in full detail.

## Contents

- [Decode vs Encode](#decode-vs-encode)
- [Menu Bar: Left Side](#menu-bar-left-side)
- [Menu Bar: Right Side](#menu-bar-right-side)
- [Results Panel](#results-panel)
- [Trace Panel](#trace-panel)
- [Decoding a Reference](#decoding-a-reference)
- [Understanding the Results Panel](#understanding-the-results-panel)
  - [Reference Section](#reference-section)
  - [Location Reference Points](#location-reference-points)
  - [Decoded Segments](#decoded-segments)
  - [Location Trimming and the Asterisk Marker](#location-trimming-and-the-asterisk-marker)
  - [Diagnostics and AI Assistance](#diagnostics-and-ai-assistance)
  - [Export GeoJSON](#export-geojson)
- [Understanding the Trace Panel](#understanding-the-trace-panel)
  - [Trace Panel Controls](#trace-panel-controls)
  - [Reference Summary](#reference-summary)
  - [Candidates](#candidates)
  - [Routing and A-Star Search](#routing-and-a-star-search)
  - [Offsets](#offsets)
  - [Result](#result)
  - [Forced Re-decode](#forced-re-decode)
- [Replay](#replay)
  - [Replay Controls](#replay-controls)
  - [What's Animated on the Map](#whats-animated-on-the-map)
- [Encoding a Location](#encoding-a-location)
  - [Placing and Editing Waypoints](#placing-and-editing-waypoints)
  - [The Snap Candidate Popup](#the-snap-candidate-popup)
  - [The Encode Workflow Panel](#the-encode-workflow-panel)
  - [Point Along Line](#point-along-line)
  - [Round-trip Verification](#round-trip-verification)
- [Map Tools](#map-tools)
- [Inspecting Segments and Nodes](#inspecting-segments-and-nodes)
- [Bringing Your Own Map](#bringing-your-own-map)
- [Tile Data Format](#tile-data-format)
- [AI Chat](#ai-chat)
  - [How It Works](#how-it-works)
  - [Suggested Prompts](#suggested-prompts)
  - [Example: LFRCNP Blocking](#example-lfrcnp-blocking)
  - [Example: Turn-angle-blocked Encode](#example-turn-angle-blocked-encode)
  - [Tool Categories](#tool-categories)
- [Decode Parameters](#decode-parameters)
  - [Presets](#presets)
- [Candidate Matching Field Reference](#candidate-matching-field-reference)

## Decode vs Encode

The mode toggle at the top-left of the menu bar switches between two entirely different workflows:

- **Decode** — paste an existing OpenLR string into the input at the bottom of the screen and press
  Decode. The rest of this guide covers this workflow.
- **Encode** — click "Encode ▾" to choose a location type (currently **Line** and **Point Along
  Line** are implemented; the other seven OpenLR location types are listed but disabled), then draw
  waypoints directly on the map to build a new reference. The result is automatically encoded to
  both binary formats and immediately decoded back through the same engine (a round-trip verify), so
  you can confirm it's correct before using it. Full walkthrough:
  [Encoding a Location](#encoding-a-location).

The bottom decode input bar and the menu bar's Trace/Replay/Results buttons are hidden in Encode
mode — the left panel shows the waypoint/encode workflow instead, which has its own Trace/Replay
toggles once a round-trip verify has run (see [The Encode Workflow Panel](#the-encode-workflow-panel)).
Switching between Decode and Encode mode doesn't discard either side's state — flip back and forth
freely and both resume exactly where you left them.

## Menu Bar: Left Side

- **Decode** — switches to decode mode. This is the default mode and where this guide's walkthrough
  takes place.
- **Encode ▾** — a dropdown-trigger, not a plain toggle: picking a location type both selects it and
  switches to encode mode in one click; clicking it again while already in encode mode reopens the
  dropdown so you can switch types without leaving encode mode.
- **Segments** — toggles a map overlay of the raw road-segment graph, colour-coded by Functional
  Road Class (FRC 0, the most important, through FRC 7). A small legend appears in the map's corner
  while this is on. This overlay is independent of any decode — it shows what's actually in the
  loaded tile archive, which is useful for confirming coverage or investigating why a decode failed
  to find candidates in a given area, and it makes every segment and node clickable for detailed
  inspection — see [Inspecting Segments and Nodes](#inspecting-segments-and-nodes).
- **Trace** — toggles the [Trace panel](#trace-panel). This button only appears in Decode mode; in
  Encode mode, an equivalent **Trace** button lives inside the
  [Encode Workflow Panel](#the-encode-workflow-panel) once a round-trip verify has run.
- **Replay** — toggles a step-by-step replay bar along the bottom of the map: candidate search, A\*
  routing, and offset trimming, animated live on the map exactly as the engine experienced them. Step
  forward, back, or scrub the timeline directly. Same Decode-mode-only menu placement as Trace above,
  with the same Encode-mode equivalent inside the workflow panel. Full detail: [Replay](#replay).
- **Results** — appears once a decode has been attempted; toggles the [Results panel](#results-panel).
  A badge shows the segment count on success (green) or ✗ on failure (red), so you can tell a
  decode's outcome at a glance even with the panel closed. This button doesn't appear in Encode mode
  at all — the left panel always shows the encode workflow there instead.

## Menu Bar: Right Side

- **Parameters** — opens the [Decode Parameters](#decode-parameters) panel, a full set of tunable
  values controlling how candidates are scored and routes validated.
- **Trace Level** — a three-way setting (**Off** / **Summary** / **Full**) controlling how much
  diagnostic detail the *next* decode records:
  - **Off** — no trace is recorded. The Results and Trace panels still show the parsed reference
    (LRPs, offsets) since that comes directly from the OpenLR string itself, not from a trace, but
    there's no candidate/routing detail and Replay has nothing to show.
  - **Summary** — records candidate ranking (accepted/rejected per LRP) and routing outcomes (route
    found/failed, DNP check, aggregate A\*-skip-reason counts) — enough for the Trace panel's tables
    and most diagnostic use.
  - **Full** — adds every individual A\* node expansion and edge skip, enabling the complete Replay
    animation and the expandable "A\* expanded N nodes" table in the Trace panel's Routing section.
    Full trace on a long or poorly-connected route can be large; Summary is the default.
- **AI** — opens AI/LLM settings (provider, base URL, model, API key; supports Anthropic, OpenAI,
  OpenRouter, Ollama, or a custom OpenAI-compatible endpoint). Once configured, a second **AI Chat**
  button appears next to it — see [AI Chat](#ai-chat).
- **Tile source** — points the app at any PMTiles archive — see [Bringing Your Own Map](#bringing-your-own-map).

## Results Panel

In **Decode** mode, the left slide-out panel (toggle with the panel-edge tab or the **Results** menu
button) is the at-a-glance answer: what a reference decoded to, the Location Reference Points that
produced it, and every road segment making up the result. This is the panel most of this guide walks
through in detail — see [Understanding the Results Panel](#understanding-the-results-panel).

In **Encode** mode, this same panel is replaced entirely by the waypoint/encode workflow — see
[The Encode Workflow Panel](#the-encode-workflow-panel).

## Trace Panel

The right slide-out panel is the deep-dive: which candidate segments were considered at each LRP and
why, the A\* routing result for each leg between them, and the final offset trim. This is where you
find out *why* the decoder chose what it chose, not just what it chose — see
[Understanding the Trace Panel](#understanding-the-trace-panel). In **Encode** mode, it shows this
same detail for the automatic round-trip verify decode instead of a manually pasted string — see
[Round-trip Verification](#round-trip-verification).

## Decoding a Reference

With **Decode** mode active, an input bar appears along the bottom of the screen:

1. Paste an OpenLR string into the text field (placeholder: "Paste OpenLR string (v3 or TPEG,
   base64)…"). Both supported formats — **TomTomV3 binary** and **TPEG-OLR** — are pasted the same
   way, base64-encoded; you don't need to say which one it is.
2. Press **Enter** or click **Decode**. The format is detected automatically: the decoder first
   tries TomTomV3 base64, then TPEG-OLR base64, then (as a fallback) TPEG-OLR hex.

Once a decode completes (success or failure), the **Results** menu button appears with its
success/failure badge — click it (or the `▶` tab on the left edge of the map) to open the
[Results panel](#understanding-the-results-panel) and see the outcome. On the map, a successful
decode briefly pulses the matched path green, then settles into a solid cyan line with small white
circles marking each segment boundary, and numbered markers (1, 2, 3…) at each LRP's encoded
position.

## Understanding the Results Panel

The panel has two areas: a **Reference** area at the top (its height is adjustable — drag the
divider between it and the segment list) showing the parsed OpenLR reference itself, and a
**Decoded result** area below showing what the engine matched that reference to on the map.

### Reference Section

At the top of the Reference area:

- **Format** — TomTomV3 (binary v3) or TPEG-OLR (ISO 21219-22).
- **Type** — the OpenLR location type (Line, PointAlongLine, etc.).
- **LRPs** — how many Location Reference Points the string encodes.
- **Pos. offset / Neg. offset** — for Line-type references, the trim distances applied after route
  validation (positive from the path start, negative from the path end). Each row is always shown,
  explicitly reading **N/A** when the reference didn't encode one, rather than the row silently
  disappearing — that way it's clear the field was considered, not just missing. Hover the **?** icon
  next to any field label for a one-line explanation (see also
  [Candidate Matching Field Reference](#candidate-matching-field-reference)). A trailing **\*** on an
  offset value means it's *estimated* from the encoded DNP sum rather than computed from the actual
  routed path length — this only happens when the underlying decode failed part-way and the true
  path length isn't known.
- For Point-Along-Line references, **Orientation** and **Side of road** are shown instead of offsets.

### Location Reference Points

Below the Reference summary, every LRP is listed as its own collapsible card, in order (First,
Intermediate, Last). Each card:

- Shows a colour-coded dot (green = first LRP, red = last, blue = intermediate) and a one-line summary
  when collapsed: coordinate, FRC, FOW, and (for all but the last LRP) LFRCNP.
- **Click the row itself** to zoom the map to that LRP's encoded position — useful for checking
  whether the coordinate lands on the road you expect.
- **Click the ▸/▾ arrow** to expand the card for full detail: exact coordinate, FRC (with its
  descriptive name, e.g. "FRC3 · Tertiary"), FOW (e.g. "FOW2 · Dual Carriageway"), Bearing (shown as
  a range, e.g. `191.3°–202.5°`, when the encoded interval isn't a single point — TomTomV3 bearings
  are quantised into 11.25° buckets, so this is normal, not imprecision), and — for every LRP except
  the last — DNP (distance to the next LRP, also a range for v3) and LFRCNP (the lowest road
  importance class permitted on the route to the next LRP; shown as `4 → 6` when the decoder's
  LFRCNP tolerance parameter relaxes that floor).

### Decoded Segments

Below the Reference area, the decode outcome is shown with an **Export GeoJSON** button (on success
— see [Export GeoJSON](#export-geojson)) and a summary line: segment count and the offset intervals
actually applied, e.g. `8 segments · +[59.8, 61.0] m · −[11.3, 12.4] m`. If a trace was recorded and
the [Trace panel](#understanding-the-trace-panel) is currently closed, a `⚡ Trace` link appears here
too, as a shortcut to open it.

The segment table lists every segment along the *full routed path* between the first and last LRP,
in travel order, with columns:

- **Segment Key** — the segment's stable identifier (e.g. an OSM way ID plus a split index, such as
  `6821064-1`, if the tile source provides one) or, failing that, its internal numeric ID.
- **FRC** / **FOW** — the segment's own road class and form of way (as stored in the map data, which
  may legitimately differ slightly from the LRP's encoded values — that's exactly what the decoder's
  FRC/FOW scoring tolerates).
- **Dir** — the direction the route travels along this segment: `S↔E` (bidirectional road), `S→E`
  (forward), or `S←E` (backward, i.e. the segment's stored geometry runs opposite to the direction
  the route travels it).
- **Length** — the segment's length in meters.

**Click any segment row** to highlight it on the map: the segment is drawn with a pulsing halo, the
map zooms/pans to fit it, and a popup appears showing the segment's full detail — FRC and FOW (with
names), direction, length, source tile and tile-local index, start/end node IDs, stable ID, and
internal ID. Click the same row again to clear the map highlight (the popup itself closes when you
click elsewhere on the map, or another segment).

### Location Trimming and the Asterisk Marker

A decoded **Line** location is trimmed by its positive/negative offsets — the actual covered extent
can be shorter than the full routed path between the first and last LRP, and can even fall entirely
within a single segment at either end. The segment table above always shows the *complete* routed
path for context, including segments the offsets end up bypassing entirely.

**A segment marked with a small `*`** (with a caption below the table: "bypassed by the offsets — not
part of the final location") means that segment is *not* covered by the trimmed location at all —
it was part of the route the decoder found, but every bit of it falls before the positive offset's
start point or after the negative offset's end point. This is common at the very start or end of a
route when the offset is large relative to that boundary segment's length.

The blue/green pulsing line on the map, and the `wkt` field in exported results, always reflect only
the *conservatively trimmed* extent (i.e. everything the un-starred segments plus the covered portion
of any partially-trimmed boundary segment actually cover) — never the untrimmed full path. This trim
is computed using the lower bound of the offset interval, which guarantees the drawn/exported extent
never overstates what the reference actually covers, even when the offset itself is a range rather
than an exact value (as with TomTomV3's quantised offset encoding).

### Diagnostics and AI Assistance

On a **failed** decode, the panel shows the raw engine error plus, when a cause can be inferred, a
plain-language diagnosis: a headline, supporting bullets (e.g. specific LFRCNP/FRC/turn-restriction
issues found in the trace), and concrete suggestions (e.g. "raise LFRCNP tolerance"). Below that, a
button adapts to what's missing: **Re-decode with tracing** or **Re-decode with full trace** re-runs
the decode at a higher [Trace Level](#menu-bar-right-side) so more diagnostic detail is captured; if a
full trace was already captured but the Trace panel is simply closed, it instead reads
**Open trace panel** and does exactly that, with no re-decode needed.

On a **successful** decode that's nonetheless suspicious (e.g. a routed leg of only a few meters,
suggesting two LRPs snapped to the same point), a similar warning panel appears so you don't mistake
a degenerate match for a good one.

Either way, an **AI Chat** button is available — see [AI Chat](#ai-chat).

### Export GeoJSON

The **Export GeoJSON** button (visible on a successful Line or PointAlongLine decode) downloads
`openlr-path.geojson`: a standard GeoJSON `FeatureCollection` containing one `LineString` feature per
*covered* segment (segments marked with `*` — entirely outside the trimmed extent — are excluded),
each carrying `frc`, `fow`, `direction`, and `length_m` properties. Bidirectional segments' coordinate
order is normalized to match the route's actual travel direction, so the exported geometry always
flows continuously from the start of the location to its end.

The collection's `metadata` includes the original `openlr` string, `location_type`, the
conservatively-trimmed `wkt` (the same one drawn on the map), and — re-expressed relative to the
*exported* segment list's own start/end (not the original LRP position) —
`pos_offset_from_covered_start_m` and `neg_offset_from_covered_end_m`, each a `[lb, ub]` pair rather
than a single number, consistent with how offsets are represented everywhere else in the app: always
a bounded interval, never collapsed to a midpoint estimate.

## Understanding the Trace Panel

The Trace panel only has data once a decode has run with **Trace Level** set to Summary or Full (see
[Menu Bar: Right Side](#menu-bar-right-side)) — otherwise it prompts you to re-decode at a higher
level. Every section below is collapsible independently.

### Trace Panel Controls

At the top of the panel, two controls apply regardless of what's below:

- **Summary / Full ●** — a quick toggle for the active **Trace Level**, equivalent to the menu bar's
  [Trace Level](#menu-bar-right-side) dropdown minus the Off option. Change it here, then re-decode
  to actually capture the new level of detail — flipping the toggle alone doesn't retroactively add
  detail to a trace already recorded.
- **Copy JSON** — copies the entire decode result (or, in Encode mode, the round-trip verify result)
  including its full trace, as JSON, to the clipboard. This also doubles as the way to retrieve the
  complete A\* node list when Trace Level is Full: the Routing section's own table only displays the
  first 200 nodes for rendering performance, with a note pointing back to Copy JSON for the rest.

### Reference Summary

The same parsed-reference information as the Results panel's Reference section (format, location
type, the raw OpenLR string, offsets), plus every LRP as its own collapsible row — click a row to pan
the map to that LRP, or expand it for full coordinate/FRC/FOW/bearing/DNP/LFRCNP detail.

### Candidates

One section per LRP, showing every candidate segment the decoder considered there: an **accepted**
table (candidates that passed every hard gate, ranked best-scoring first) and a collapsible
**rejected** table (candidates that failed a gate, with the specific reason — e.g. bearing off by a
given number of degrees, or too far from the search radius). Each row shows the segment, direction,
positional distance, bearing, snap position along the segment (with an `S`/`E`/`I` tag for
start/end/interior), and the full score breakdown (distance, bearing, FRC, FOW, wrong-endpoint,
interior-snap components, plus the total). **Click a segment button** anywhere in these tables to
highlight it on the map and open a detail popup, same as clicking a row in the Results panel.

Each accepted row also has a **📌 pin** button — see [Forced Re-decode](#forced-re-decode).

### Routing and A-Star Search

One section per leg (the route between one LRP and the next), showing the from/to candidate pair the
decoder tried, whether A\* found a route, the resulting path (click to highlight all its segments at
once) and length, and the DNP check (whether the routed length actually falls within the LRP's
encoded distance-to-next-point interval). When Trace Level is Full, an expandable table lists every
individual A\* node expansion (node, via-segment, g/h/f costs) and every edge the search skipped, with
the reason (FRC below LFRCNP floor, wrong direction on a one-way, turn restriction, over max
distance, or too sharp a turn).

### Offsets

The positive/negative offset intervals actually applied to trim the route, in the same form shown
elsewhere: an exact value when the interval collapses to a point, or a `[lb, ub]` range otherwise.

### Result

A compact restatement of the outcome (segment count, offsets) with **Copy WKT** and **Copy GeoJSON**
buttons — quick clipboard access to the same conservatively-trimmed geometry the Results panel's
Export GeoJSON produces, without needing to save a file.

### Forced Re-decode

The **what-if** tool: force the decoder to route through specific candidates you choose, instead of
whatever it picked on its own, without needing a different input string.

- From any LRP's Candidates table, click a candidate row's 📌 button to **pin** it — or, whenever
  every LRP has at least one accepted candidate, click **📌 Pin best candidates** (in the bar that
  appears below the last Candidates section) to pin the top-ranked candidate at *every* LRP in one
  click. This shortcut is available whether or not you've pinned anything by hand yet — it's a fast
  way to set up a baseline forced decode before hand-tweaking individual pins.
- Once at least one LRP is pinned, an **N/M pinned** counter and a **✕ Clear pins** button appear in
  that same bar. Once *every* LRP is pinned, a **▶ Re-run with pinned candidates** button appears too.
- **Re-run with pinned candidates** executes A\* using exactly the pinned segments at each LRP,
  bypassing the decoder's own candidate search entirely, and reports the outcome in a
  **Forced Decode Result** section: success/failure, segment count, a truncated WKT preview with its
  own copy button, and a button to highlight the forced path's segments on the map.
- A note in that result also states whether that exact from/to segment pair at each leg was ever
  actually attempted by the *original* decode (as recorded in its own routing trace) — so you can
  tell a genuinely infeasible path (A\* tried it and failed) apart from one the original decode simply
  never got around to trying.
- Pins persist across trace-level changes and stay visible even if you collapse the Candidates
  sections; **Clear pins** removes all of them and discards the forced result at once.

## Replay

Step through a decode (or an encode's [round-trip verify](#round-trip-verification)) one recorded
event at a time, watching the map animate exactly what the engine did — candidate search, A\* node
expansion, and the final route — instead of only ever seeing the finished result.

Open it with the **Replay** menu button (Decode mode), or the **Replay** button inside the
[Encode Workflow Panel](#the-encode-workflow-panel) (Encode mode, once a verify has run). It needs
**Trace Level** Summary or Full (see [Menu Bar: Right Side](#menu-bar-right-side)) — Off records
nothing to step through.

### Replay Controls

- **◀ / ▶** step one recorded event at a time; the left/right arrow keys do the same.
- A **timeline bar** shows overall progress with colored **phase strips**: one per LRP's candidate
  search, one per leg's A\* search, and a final strip for the outcome (green on success, red on
  failure). Click or drag anywhere on the timeline to jump straight to that point instead of
  stepping through one event at a time.
- The step counter also reports the total A\* node count once Trace Level is Full.
- A status line describes the current step in plain language — e.g. "LRP 1 — searching within 30 m",
  "Leg 1 — A\* · 42 nodes · g=180m h=25m", "Leg 1 — route found · 289 m · 8 segs", "Positive offset —
  60 m".
- If Trace Level is Summary rather than Full, a hint reminds you: "Partial replay — A\* node
  expansion map not shown" — the node-by-node animation below specifically needs Full.

### What's Animated on the Map

- **Candidate search** (per LRP) — a circle showing the search radius appears first, then every
  candidate as a colored dot: accepted candidates one color, rejected ones colored by *why* they
  failed (bearing / radius / score / other) — the same accepted/rejected data shown in the Trace
  panel's [Candidates](#candidates) tables, just animated in the order the engine actually evaluated it.
- **A\* routing** (per leg, Full trace only) — expanded nodes appear one batch at a time, each
  colored along a blue → cyan → yellow → red gradient by its accumulated path cost relative to the
  leg's worst-case cost, so as the search fans out, expensive detours visually stand out from cheap
  branches close to the start. The most recently expanded nodes are highlighted as the active search
  frontier.
- **Route found** — the winning path's segments and the leg's snap points are drawn once A\* succeeds
  for that leg.
- **Offset trimming** — no separate map animation; the status line reports the offset applied, and
  the final trimmed result is the same path already shown in the
  [Results panel](#understanding-the-results-panel).

## Encoding a Location

Switch to **Encode** mode (see [Decode vs Encode](#decode-vs-encode)) to build a brand-new OpenLR
reference from a location you draw on the map, rather than decode one you already have. Waypoints
are drawn directly on the map; the left panel — the [Encode Workflow Panel](#the-encode-workflow-panel)
— is where you review them and trigger the actual encode.

### Placing and Editing Waypoints

The **right mouse button** is dedicated entirely to waypoint editing; left click/drag stays plain map
panning throughout Encode mode.

- **Right-click empty map** — for a **Line**, appends a new waypoint at the end of the route; for
  **Point Along Line**, places (or replaces) the single point.
- **Right-click directly on the already-drawn route line** (Line only) — inserts a new via-point at
  that position, splitting the leg it lands on.
- **Right-click a numbered waypoint marker** — moves that waypoint. (A plain **left-click** on a
  marker instead **removes** it immediately, no confirmation.)

Any of the three can be a right-click-drag rather than a plain click: a dashed "ghost" line previews
the pending edit live as you drag, and releasing the button — with or without having moved the mouse
— always opens the [snap candidate popup](#the-snap-candidate-popup) at the release point. This is a
deliberate two-step action (commit the rough position, then confirm exactly where to snap), not a
single-click shortcut. Press **Escape** at any point mid-drag to cancel without opening the popup.

Two more ways to edit the waypoint list, both in the [Encode Workflow Panel](#the-encode-workflow-panel):

- **▲ / ▼** next to a waypoint row reorders it (Line only — order is meaningless for a single PAL point).
- Paste **"lon,lat" pairs**, one per line, into the textarea and click **Set Waypoints** to replace
  the entire waypoint list at once — a faster path than clicking through a long route by hand.

**Undo** keeps a full history, not just one step back — every add/insert/move/remove/"Set Waypoints"
pushes the prior list, and Undo pops the most recent one off. **Clear** wipes the waypoints and
discards any encode/verify result already produced. Switching location type (Line ↔ Point Along
Line) via the Encode dropdown also clears everything — a multi-waypoint route and a single point
aren't interchangeable representations.

### The Snap Candidate Popup

Opens at the release point of every waypoint edit above. A waypoint you click is not itself an LRP —
it's just where the encoder starts looking for a real road to anchor to — so this popup exists to let
you choose precisely which nearby road or intersection to use, rather than silently picking the
nearest one:

- Candidates come from the **encoder's own loaded graph** (independent of whatever tiles the decoder
  side has loaded) — tiles near the click point are fetched first if needed, so a fresh area doesn't
  falsely show "nothing found" just because nothing has loaded there yet.
- Each candidate lists as either **Intersection** (a graph junction node) or a road segment (its
  stable ID or segment number, FRC, and distance in meters). Selecting one redraws the **actual
  routed preview** for that specific choice — not just a straight line to it — so you can compare how
  the real route differs before committing.
- If nothing is nearby: "No roads found very close by — will snap to the nearest available road,"
  rather than blocking you on an empty list.
- For **Point Along Line**, **Orientation** and **Side of road** selectors are embedded directly in
  this same popup, since there's only ever the one point/click to set them on — see
  [Point Along Line](#point-along-line).
- For **Line**, a **Last waypoint** checkbox — check it before confirming to immediately open the
  Encode Workflow Panel and kick off the actual encode, without a separate manual step afterward.
  Disabled until the route would have at least 2 waypoints.
- **Enter** commits the selected candidate (or the bare click position, if none was selected);
  **Escape** or clicking away cancels without changing anything.

### The Encode Workflow Panel

Docked in the same left-hand slide-out the [Results panel](#understanding-the-results-panel) occupies
in Decode mode.

- The current waypoint list, each row with its coordinate and (for Line) reorder/remove controls.
- A **live, not-yet-encoded route preview**: as waypoints are added, a segment table — the same
  columns as the decode Results panel's (Segment Key, FRC, FOW, Dir, Length) — and a total
  length / lowest-road-class-on-route summary update automatically. You don't need to actually encode
  first to see what the route will look like.
- The same paste-in textarea and **Set Waypoints** / **Undo** / **Clear** controls described above.
- **Point Along Line** only: **Orientation** / **Side of road** selects, sharing the exact same
  underlying value as the map popup's own selects — change it in either place and the other reflects it.
- The **Encode** button (labeled "Encode (Line)" or "Encode (PAL)") — disabled until there's enough
  to encode: at least 2 waypoints with a valid live route for Line, or at least 1 point for
  PointAlongLine.
- Once encoded, the **v3** and **TPEG** output strings each appear with their own **copy** button —
  and since these are exactly the strings you'd paste in to decode, **clicking the value itself**
  jumps straight to Decode mode and decodes that exact string immediately, so you can see your new
  reference through the decoder's own eyes without retyping anything.

### Point Along Line

A **Point Along Line** location has exactly one waypoint — placing a second one replaces the first
rather than extending a route. Two attributes, set via the map popup or the workflow panel (they
share the same value):

- **Orientation** — direction of travel through the point, relative to the two ends of its segment:
  `NoOrientation`, `FirstTowardSecond`, `SecondTowardFirst`, or `BothDirections`.
- **Side of road** — which side of the road the point represents: `DirectlyOnOrNA`, `Right`, `Left`,
  or `Both`.

There's no offset/DNP concept here — a single point has nothing to trim or route between.

### Round-trip Verification

Every encode immediately re-decodes its own **v3** output through the exact same decode engine used
everywhere else in this app — a genuine round trip, not a simulated check — and reports the outcome
as a badge: **✓ round-trip verified**, or **⚠ verify failed: …** with the reason. **Trace** and
**Replay** buttons appear alongside the badge once this verify has run, opening the same
[Trace panel](#understanding-the-trace-panel) and Replay bar used for ordinary decoding, just fed from
this automatic verify-decode instead of a manually pasted string.

Treat a verify failure as seriously as a decode failure: it means the encoder produced a reference
that the decoder itself — the very same engine — can't reliably resolve back to the route you drew,
which would fail identically for anyone else decoding that string later.

The **Max inter-LRP dist.** field in [Decode Parameters](#decode-parameters) is the one
encoder-specific tuning knob here: it caps how far apart two waypoints can be before the encoder
splits them into an intermediate LRP automatically, keeping every leg within that distance.

## Map Tools

A collapsible toolbar (the **⚙** button, bottom-right of the map) holds five independent tools, each
toggled on/off — only one is meaningfully active at a time, and **Escape** cancels whichever is
active:

- **📍 Capture coordinates** — click anywhere on the map (or press **Enter**) to copy that point's
  `lat, lon` straight to the clipboard; a small readout follows the cursor while active. After
  copying, a popup offers **Add pin** to drop a persistent, labeled marker there too.
- **🔍 Zoom to coordinates** — type `lat, lon` (comma- or space-separated) and click **Go** (or press
  Enter) to fly the map there and drop a pin automatically.
- **📏 Measure distance** — click to add points along a path; the running total (plus the live
  segment out to your cursor) is shown as you go. Double-click to finish; click the tool again to
  clear it.
- **🧭 Measure bearing and distance** — click a start point, then an end point, to see the compass
  bearing and straight-line distance between them; click again to start a fresh measurement.
- **🔗 Copy permalink** — copies a URL that reopens this app with the *current OpenLR string*
  pre-filled and automatically decoded on load (`?olr=<string>`) — a quick way to share exactly what
  you're looking at, or bookmark it for later. Old links using the previous `#q=<string>` hash
  format still work too.

Every pin dropped by the coordinate tools (capture or zoom-to) shows its own popup with **Dismiss**
and **Dismiss all** buttons, and stays on the map until dismissed.

A **basemap selector** (also bottom-right) switches between several free vector/raster styles
(Liberty, Bright, Positron, OSM, Carto Light/Dark, Satellite, Maptoolkit) — purely cosmetic, entirely
independent of whichever OpenLR tile source is configured for decoding/encoding (see
[Bringing Your Own Map](#bringing-your-own-map)).

## Inspecting Segments and Nodes

With the **Segments** overlay on (see [Menu Bar: Left Side](#menu-bar-left-side)), every road segment
and junction node becomes clickable directly on the map — independent of any decode, so you can
explore the loaded map data on its own terms:

- **Click a segment** to open a detail popup: FRC and FOW (with names), direction, length, source
  tile and tile-local index, start/end node IDs, stable ID, and internal ID — the same information
  shown when clicking a segment row in the [decode Results panel](#decoded-segments), just reachable
  without decoding anything first. The segment is also highlighted on the map (a filter-based
  highlight on the Segments layer itself — distinct from the pulsing halo used for a segment that's
  actually part of a decode result).
- **Click a node** (a road junction) to open a similar popup: latitude/longitude, source tile and
  tile-local index, stable ID, and internal ID.

This is especially useful for diagnosing a decode that unexpectedly found no candidates near an
LRP — turn the Segments overlay on and look directly at what road data (if any) actually exists
there, rather than guessing from the decode's error message alone.

## Bringing Your Own Map

This tool isn't tied to one map provider. The **Tile source** menu (top-right) points the app at any
[PMTiles](https://protomaps.com/) archive you build or host yourself — TomTom, OpenStreetMap,
Overture, ESRI, or anything else — for both decoding and encoding. Enter a base URL and click
**Apply & reload**; the page reloads so the new archive's manifest and tiles are fetched fresh from
scratch. There's no server-side component beyond serving that one archive (plus, in this deployment,
a small same-origin proxy in front of it).

Two more ways to point at a specific archive without touching that menu:

- A **`?tiles=`** URL query parameter (e.g. `?tiles=europe`) — resolved against the same base URL,
  useful for linking directly to a specific sub-archive without changing the stored default.
- The **🔗 Copy permalink** [map tool](#map-tools) — reopens the app with the current OpenLR string
  pre-filled and decoded, using whichever tile source is already configured.

## Tile Data Format

The map data behind decoding and encoding — the road segments, junctions, and turn restrictions —
comes from a [PMTiles](https://protomaps.com/) archive: one large, static file served over plain
HTTP range requests, with no database or API server behind it. Inside that archive, each tile
holds a custom binary payload (not the vector-tile/MVT format most map viewers use) purpose-built
for this app: a list of road segments (each already split at every junction, so a segment always
runs node-to-node with nothing branching off it partway through), a list of junction/end nodes,
and a table of turn restrictions. Every segment and node carries its **FRC** (Functional Road
Class, 0=motorway down to 7=minor/local) and, for segments, **FOW** (Form of Way — motorway,
roundabout, slip road, etc.) — the same two attributes an OpenLR reference itself encodes, which
is exactly what the decoder compares candidates against. All tiles sit at one fixed zoom level
(not a multi-level pyramid the way basemap tiles usually are) — chosen so that a handful of tiles
around any point comfortably covers even a generous candidate search radius.

Every segment and node also carries a **stable ID**: an opaque text string — an OSM way ID, a
database key, a UUID, whatever the archive's builder chose — shown throughout the UI (segment
popups, the Results/Trace panels' segment tables) as the **Segment Key**. It's meaningful only in
the sense that the same real-world road keeps the same ID across archive rebuilds; this app treats
it as an opaque label, not something to parse. If you build your own archive (see
[Bringing Your Own Map](#bringing-your-own-map)), the full format spec and build pipeline live in
this project's own documentation repos, not here — this section is just enough to make sense of
what you're looking at on screen.

## AI Chat

Once an AI provider is configured (see [Menu Bar: Right Side](#menu-bar-right-side)), the **AI Chat**
button opens a draggable chat panel (drag its header to move it out of the way of whatever you're
looking at). It's a genuine diagnostic assistant, not a chatbot guessing from whatever's on screen —
every answer is grounded in real tool calls into the live decoder/encoder and road graph, not
invented or estimated.

### How It Works

- Every question is sent alongside a **"Current decode data" / "Current encode data"** block — a
  compact summary of the active result (outcome, segment counts, active parameters, and
  pre-computed "key signals" like A\*-skip ratios) — so straightforward questions are answered
  instantly with no tool calls at all.
- For anything deeper, the assistant calls **tools** that query the live engine directly: full
  per-candidate score breakdowns, complete A\* expansion stats, road-graph topology at a specific
  junction, whether a specific segment sequence is even traversable, and more — see
  [Tool Categories](#tool-categories) below. A small **tool activity strip** above the chat shows
  which tools ran and how many bytes of data moved, so it's visible when — and how much — it's
  actually looking something up.
- Responses **stream in live**, token by token, with a typing indicator while a response is starting.
- Most diagnostic answers follow a structured format — **What happened** (one sentence), **Why**
  (bullets citing specific numbers from the data), **Suggestions** (concrete parameter changes) — the
  suggested prompts below are pre-wired to request exactly this format.
- The assistant can **embed live SVG diagrams** directly in its answer — bearing wedges, score bar
  charts, DNP number lines, junction sketches — each with **Copy SVG** and **Export PNG** buttons and
  a click-to-expand zoom.
- The assistant can act on your behalf, too: highlight segments, pan/zoom the map, pin candidates,
  and trigger a forced re-decode — so asking it to point something out can result in the map actually
  panning there and highlighting it, not just a text description of where to look.
- **↺** clears the conversation and starts fresh.

### Suggested Prompts

Four ready-made prompts appear as clickable chips — once as a starting point when the chat is empty,
and again (in a smaller row) after every reply — tailored to the current mode and outcome:

| Situation | Prompts |
|---|---|
| Decode failed | "Why did this fail?" · "What is the minimum parameter change to fix this?" · "Is this an encoder or decoder problem?" |
| Decode succeeded | "Summarise this decode" · "Are there any suspicious aspects?" · "Why was this route chosen over alternatives?" |
| Encode failed | "Why did this fail to encode?" · "Is this a turn-angle problem or a genuine disconnection?" · "What is the minimum parameter change to fix this?" |
| Encode succeeded | "Summarise this encode" · "Did the round-trip verify catch any issues?" · "Why did the encoder choose this path?" |

Typing anything else instead works exactly the same way — these are just a fast starting point, not
the only questions it understands.

### Example: LFRCNP Blocking

Asking **"Why did this fail?"** against a decode whose trace shows only 4 A\* nodes expanded with 52
edges skipped for exceeding the LFRCNP floor produces something like:

> **What happened:** Routing failed because the LFRCNP floor blocked nearly all candidate edges
> before A\* could explore the graph.
>
> **Why:**
> - Only 4 nodes were expanded before the search exhausted its reachable set
> - 52 edges were skipped because their FRC exceeded the LFRCNP floor — a 13:1 skip-to-expansion ratio
> - This pattern (high FRC-skip ratio, very few expansions) is the signature of an LFRCNP problem, not
>   a bearing or distance one
>
> **Suggestions:** Increase LFRCNP tolerance by 1–2 steps to allow connector and service roads into
> the search.

### Example: Turn-angle-blocked Encode

Asking **"Is this a turn-angle problem or a genuine disconnection?"** after a failed encode — where
the assistant calls `diagnose_waypoint_connection` on the failing leg — might produce:

> **What happened:** The encoder could not connect these two waypoints under the current turn-angle
> cap, even though a path between them exists.
>
> **Why:**
> - A path exists with no turn-angle limit (340.5 m), but not within the current 150° cap
> - The blocking junction requires a 178.4° turn to continue — essentially a straight reversal, most
>   likely a dead end
>
> **Suggestions:** Raise the turn-angle cap if this reversal is a real, intended maneuver, or redraw
> the route to avoid that dead end.

### Tool Categories

The assistant chooses which tools to call based on your question — you never call them directly.
Roughly:

- **Decode-trace tools** — decode outcome/summary, the exact parsed reference (bearing/DNP/LFRCNP
  intervals), full per-LRP candidate scores (including rejected candidates and why), per-leg A\*
  stats, and the routed segment list.
- **Graph inspection tools** — a segment's full attributes/geometry, every segment near a coordinate
  (useful when an LRP found no candidates at all), and a segment's connected neighbors at each
  endpoint (junction topology, turn restrictions).
- **Forced-decode tools** — the same pin-and-re-run [what-if capability](#forced-re-decode) available
  in the Trace panel, callable directly: pin candidates, re-run, inspect the result, and list every
  candidate combination the *original* decode attempted.
- **Path analysis tools** — check whether a specific sequence of segments is even traversable under
  current constraints, score a proposed path's length against the DNP window, inspect full junction
  topology, or get a full bearing analysis for a specific candidate.
- **Map control tools** — highlight segments, pan/zoom to a coordinate, or jump straight to an LRP.
- **Encode diagnostic tools** (Encode mode only) — encode outcome/summary, the drawn waypoint list,
  the encoder's own graph inspection, waypoint-connection diagnosis (genuine disconnection vs. a
  turn-angle rejection), Rule-4 boundary-expansion replay, and a raw turn-angle check between two
  segments at a node.

If a round-trip verify has run, every decode-side tool above also works against that verify result —
so after an encode, you can ask decode-flavored questions like "why did LRP 2's verify pick this
candidate?" too.

## Decode Parameters

Every tunable value the decoder uses to score candidates and validate routes. Hard tolerances (gates
that reject a candidate/route outright) and soft weights (penalties that only affect ranking) are
kept as distinct concepts throughout — a decode is always `(string + this tolerance profile) → path`,
and both are worth recording together for reproducibility. Click the **?** next to any field in the
live Parameters panel for the same detail shown below, in place.

Two **advanced penalty tables** (FRC × FRC and FOW × FOW, hidden behind an "Advanced" toggle) let you
tune the exact penalty for every specific pair of road classes/forms — e.g. how harshly a motorway
LRP is penalised for matching a service road candidate, independent of the general FRC weight above.

You can also **save the current parameter set** under a name for later reuse, **load** a previously
saved set, or **reset** everything to the built-in defaults.
