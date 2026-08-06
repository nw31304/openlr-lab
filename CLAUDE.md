# CLAUDE.md — OpenLRLab

Browser-based WebAssembly OpenLR **diagnostic decoder and encoder** with global coverage. This repo
owns *only* the browser UI (React/MapLibre) — it contains no Rust code at all. Every bit of OpenLR
logic, including the `openlr-wasm` `wasm-bindgen` adapter this UI calls into, lives in a separate
repo, [openlr-core](https://github.com/nw31304/openlr-core), which this repo depends on for its
compiled wasm module (built there, copied here — see §1). Map data is preprocessed once per
source-data release into a static PMTiles archive (R2/CDN); no live server queries at runtime. Two
formats, both decode and encode: **OpenLR binary v3** (TomTom; 11.25° bearing buckets, ~58.6 m DNP
buckets) and **TPEG-OLR / ISO 21219-22** (full precision).

**Read §2 before writing any code — several invariants fail silently (wrong output, not a crash).**
**Most of the correctness-critical invariants now live in `openlr-core`'s own `CLAUDE.md`** — this
repo's list below is only what's genuinely specific to the browser/wasm side.

---

## 1. Relationship to openlr-core

`openlr-core` owns every line of OpenLR-aware Rust: `openlr-codec`/`openlr-graph`/`openlr-engine`/
`openlr-encoder`/`openlr-provider` (the engine, zero `wasm-bindgen`/browser awareness) and
`openlr-wasm` (the thin `wasm-bindgen` adapter — JSON-in/JSON-out glue, the tile-injection protocol,
nothing algorithmic) — `openlr-wasm` used to live in this repo but moved to `openlr-core` once it
became clear it carried no client-specific shaping (see that repo's `CLAUDE.md` §1 for the history).
This repo owns `web/` and nothing else. If you find yourself wanting to add real encoding/decoding
logic — or even wasm-bindgen JSON-shaping glue — here, it belongs in `openlr-core` instead; there is
no Rust workspace in this repo to add it to.

**Local dependency setup**: this repo has no `Cargo.toml`, no `crates/` directory, no `cargo build`
step of any kind. The wasm module `web/` actually loads (`web/src/wasm/`, gitignored, never
committed) is built by running `wasm-pack build` *from `openlr-core`'s own
`crates/openlr-wasm`*, pointed at this repo's `web/src/wasm` via `--out-dir` — see the README's
Build section for the exact command, and `openlr-core` cloned as a sibling directory of this repo
(`../openlr-core`) is a hard prerequisite for it to resolve. If `web/src/wasm` is stale or missing,
the fix is always to re-run that command in the sibling checkout, never to add Rust code here.

If this repo's docs and `openlr-core`'s ever describe the same invariant differently, that's drift
to fix — `openlr-core`'s own `CLAUDE.md` is authoritative for engine *and* wasm-binding behavior;
this repo's mentions of it should be describing the same reality from the consumer's side, not a
second opinion.

---

## 2. Critical Invariants (browser/wasm-side only)

1. **Licensing depends on the configured source data.** OSM-derived sources (OSM directly, or any
   provider whose road-network theme derives from OSM) carry **ODbL** — attribution and
   share-alike obligations apply to the derived tile store. Verify the actual license of whichever
   source feeds a given build. See §13.

2. **JS owns all I/O — WASM stays synchronous.** `openlr-core`'s engine is synchronous Rust with no
   async-trait across FFI; `openlr-core`'s `openlr-wasm` crate operates over an in-memory tile cache
   that JS populates. When the engine needs a tile it yields a tile-key request to JS; JS fetches
   and resumes with bytes injected via `load_tile()`. This is a browser/wasm-boundary design
   decision specific to that binding, not a concern of the engine crates themselves — the
   `Graph`/`OpenLrDataProvider` API is agnostic to how a caller gets tile bytes into it
   (`openlr-core`'s own `openlr-cli` reads them straight off local disk, no injection dance needed).
   This repo's role is purely the JS side of that protocol: `web/`'s `TileLoader`/store code fetches
   tile bytes over HTTP range reads and calls `load_tile()`.

3. **Candidate-search tile-prefetch completeness across a `retry_decode` param change.**
   `prefetch_tile_keys()` sizes its per-leg corridor buffer from `candidate_search_radius_m` *as of
   the `start()` call*. `Decoder::retry_decode` (in `openlr-core`'s `crates/openlr-wasm/src/lib.rs`)
   lets a caller override `DecodeParams` — including `candidate_search_radius_m` — for a fresh
   decode against the *already-loaded* graph. `retry_decode` recomputes `prefetch_tile_keys` for
   the *merged* params first and returns `{ "needs_tiles": [[z,x,y], ...] }` instead of decoding if
   any aren't loaded yet; this repo's LLM chat's `retry_decode` tool (`web/src/llm/tools.js`)
   fetches them via `storeActions.fetchAndLoadDecodeTiles` and retries, capped at 5 rounds. The
   underlying "why does this matter at all" invariant (candidate search has no reactive fallback the
   way A\* interior routing does) is `openlr-core`'s Invariant 10 — this entry is specifically about
   the one place in *this* repo's JS code that has to uphold it across a parameter change
   mid-session.

4. **Decode-time `zoom` must always come from the archive's own manifest, never a hardcoded
   literal.** `web/src/App.jsx` throws (surfaced through the setup-error UI) if a manifest is
   published missing both `tile_zoom` and `zoom`, rather than silently substituting `12` — every
   `TileKey` computation downstream depends on this matching the zoom the archive was actually
   built at. (`openlr-core`'s native path never had this exposure — `PmtilesProvider` reads the
   zoom directly from the PMTiles binary header, no side-channel manifest field involved.)

---

## 3. Architecture

```
  BUILD TIME (few times/year — separate repo: openlr-pmtiles)
  Road network source data ──▶ [openlr-pmtiles-build] ──▶ PMTiles archive ──▶ R2 + CDN

  RUNTIME (browser only, no server)
  PMTiles ──range reads──▶ [TileLoader] ──▶ [OpenLRDataProvider] ──▶ in-memory graph   ┐
                                                    │                                   │
  OpenLR string ──▶ [Codec: v3/TPEG] ──▶ unified LRP model ([LB,UB] intervals)         │ openlr-core
                                                    │                                   │ (external repo —
                                        [Engine: candidates + A* + validation]          │  engine AND the
                                                    │                                   │  openlr-wasm
                                        [Encoder: Line/PAL, waypoint snapping]          │  bindings crate
                                                    │                                   │  both live there)
                                     openlr-wasm (openlr-core — thin wasm-bindgen glue)  ┘
                                                    │
                                    compiled wasm module, copied into web/src/wasm
                                                    │
                                        [Diagnostics + MapLibre UI]  (this repo — web/)
```

`openlr-core`'s `openlr-wasm` crate depends on that repo's own five engine crates and adds nothing
algorithmic — JSON DTOs, the tile-injection protocol above, and thin per-method wrappers. This
repo's `web/` (Vite + React + MapLibre GL JS) is the actual product, and the only thing this repo
contains.

The PMTiles builder lives in a separate repo,
[openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles) (private) — `openlr-core` (not this
repo) is the consumer of the archives it produces. The only contract between those two repos is
the tile **format** itself (§4–5 below); `openlr-core`'s `openlr-provider` decoder must be updated
whenever that format changes in `openlr-pmtiles`. This repo has no code that understands the tile
*binary* format at all anymore — `load_tile()` just hands opaque bytes through to `openlr-core`.

---

## 4. Data model (owned by openlr-core now — summary only)

The full segment/node/restriction table layouts and the tile binary format (§5) are defined by
`openlr-core`'s `openlr-graph`/`openlr-provider` crates — see that repo's own `CLAUDE.md` §4–5 for
the authoritative byte-level spec if you're touching the format itself. Kept here only as
orientation for debugging this repo's UI:

- Each segment is post-split (node-to-node only — no interior branches); identity is its array
  index within a tile. Every segment/node also carries an opaque, provider-defined **stable ID**
  string (an OSM way ID, a UUID, a database key, etc.) — never a hash, never parsed by this repo,
  shown in the UI as the "Segment Key."
- Node table entries flag boundary nodes (present in more than one tile), stitched across tiles by
  stable ID.
- Turn restrictions reference segments/nodes by local index (intra-tile) or stable ID (cross-tile).

---

## 5. Tile format (owned by openlr-core now — summary only)

Custom binary payload (magic `OLRL`, version 3), not MVT — a single fixed zoom level (`z/x/y` is
purely an addressing convention, not a level-of-detail pyramid), coordinates at 1e-7° precision.
`openlr-core`'s `openlr-wasm::TileLoader.load_tile()` receives these bytes over the wire (fetched by
this repo's `web/` code via HTTP range reads) and passes them straight to `openlr-core`'s parser,
unmodified — see that repo's `CLAUDE.md` §5 for the full header/segment/node/restriction-table byte
layout.

---

## 6. Build pipeline

Lives entirely in the separate [openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles)
repo — not in this one, and not in `openlr-core` either. See that repo's own docs
(`PreProcessing.md`) for pipeline internals, schema config, CLI reference, and open TODOs. Nothing
here should reference `pipeline/` paths or the pipeline binary directly.

---

## 7. Codec layer (owned by openlr-core)

`openlr-codec`'s `Lrp`/`LocationReference`/`CircularInterval`/`LinearInterval` types are the
unified model every layer of this repo's UI ultimately renders — see `openlr-core`'s `CLAUDE.md`
§7 for the full type definitions and the v3-vs-TPEG interval-filling distinction. Nothing in this
repo re-implements or wraps codec logic; `openlr-wasm` calls straight through.

---

## 8. Decode engine (owned by openlr-core)

See `openlr-core`'s `OpenLREngine.md` for the full design reference (candidate selection, scoring,
A\*, validation) — that file moved there along with the crate it describes. This repo's role is
purely: call `decode()`/`decode_forced()`/`retry_decode()` through the `openlr-wasm` binding, and
render whatever `DecodeTrace` comes back (`web/`'s Trace/Replay panels, the LLM chat's tools).

---

## 9. Decode parameters

`DecodeParams` (`openlr-core`'s `crates/openlr-engine/src/params.rs`) is the source of truth for
the full field list and every field's hard-gate-vs-soft-weight classification — don't re-enumerate
it here. What *is* this repo's concern: the three named presets (Permissive/Default/Strict) shown
in the Parameters panel are **not** hand-copied in JS — `web/src/store.js`'s `PRESETS` object is
populated once at startup from `Decoder.list_presets_json()` (a wasm-bindgen call into
`DecodeParams::preset()`), specifically so this repo's UI can never silently drift from
`openlr-core`'s own preset values the way it once did. `web/src/components/ParamsPanel.jsx`'s
`SCALAR_FIELDS`/`EXTRA_FIELDS` lists are this repo's own UI-field metadata (labels, units, slider
ranges) layered on top of that — keep them in sync with `DecodeParams`'s actual field list if a
field is ever added or removed there.

---

## 10. Diagnostics (the differentiator)

See `Diagnosis.md` for the full decode-failure taxonomy (every hard-error and silent-misdecode
class, trace event fields, and which are auto-diagnosed today vs. still manual) — this section is
the summary. This is genuinely this repo's own doc, not `openlr-core`'s — it's about what the UI
surfaces to a user, not the engine's internal behavior.

1. **Stepped debugger:** candidate radius per LRP; pass/fail colours with specific reason;
   A* frontier animation; badge where path breaks.
2. **Interval visualization:** bearing wedge (wide v3 / narrow TPEG), DNP band, τ/δ halos.
3. **Desired-vs-actual explanation:**
   - Forced-decode mode is **implemented**: pin a candidate per LRP in the TracePanel (or via the
     LLM chat's `set_pinned_candidates` + `run_forced_decode` tools) and re-run A* against just
     those pins, to test directly whether a desired path is feasible and see its score table next
     to the winning path's.
   - The rest of this item is **not implemented** — still the target design, not current
     behavior:
     - Automatically diff against the chosen path at its divergence node.
     - Classify: **infeasible** (direction / turn restriction / LFRCNP / DNP / not generated /
       search limit) or **feasible-but-outscored** (attribute margin per term, per LRP).
     - **Root-cause verdict:** decoder-tunable vs. encoder-deficient — this would need a
       closed-form/LP analysis living in `openlr-core` (the cost function's own additivity is what
       would make it tractable — see that repo's Invariant 6), surfaced through this repo's UI.
     - Today the closest substitute is the LLM chat reasoning manually over the trace
       (see `WebFrontend.md`).

---

## 11. Encoder

The actual encoding algorithms (Line/PointAlongLine assembly, Rule-1/4, waypoint snapping and
inter-waypoint routing) live in `openlr-core`'s `openlr-encoder` crate now — see that repo's
`CLAUDE.md` §2 Invariant 9 and `OpenLREngine.md` for the algorithm detail. This repo's role:

- **Waypoint placement UI** (`Map.jsx`): right-click to append/insert/move a waypoint, the
  snap-candidate popup, the live route preview — all backed by `openlr-core`'s `openlr-wasm` crate's
  thin `Encoder` methods (`route_between`, `candidates_near_point`, `encode_line`, `encode_pal`),
  which call straight into that repo's `openlr-encoder::waypoint` module and JSON-shape the result.
- **Round-trip verification**: every encode in the UI immediately decodes its own output (both v3
  and TPEG) through the ordinary decoder — this *is* a real decode, so it drives the same
  Segments/Trace/Replay panels the decode side already has, unmodified.
- **Diagnostics** (`openlr-core`'s `diagnose.rs`/`expansion.rs`, called through its own
  `openlr-wasm` crate): `diagnose_connection` distinguishes genuine disconnection from being blocked
  specifically by the turn-angle gate; `check_boundary_expansion` replays Rule-4 expansion in
  isolation. Both are exposed as LLM chat tools (`web/src/llm/tools.js`, `SYSTEM_PROMPT.md`) for the
  same reason the decode side's trace-drilldown tools are.

---

## 13. Licensing & attribution (non-negotiable)

Licensing depends entirely on the configured source data — verified at build time in
[openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles), not here. Sources derived from OSM
(OSM directly, or any provider whose road-network theme is OSM-derived, e.g. Overture) carry
**ODbL**: the derived tile store and all served output must preserve attribution and honour
share-alike obligations. Document exact attribution text before public release.

---

## 14. Native (non-wasm) use

Lives entirely in [openlr-core](https://github.com/nw31304/openlr-core) now — its own `openlr-cli`
crate is a batch decode binary against a local `.pmtiles` archive, with no dependency on anything
in this repo. This repo has no native/CLI ambitions, and no Rust code, of its own — it's the
browser UI.

---

## 15. Agent conventions

- **All decode/encode logic, and the wasm-bindgen adapter itself, belong in `openlr-core`, not
  here.** This repo has no `crates/` directory and should never gain one — if a change here starts
  looking like it needs new Rust code (a graph algorithm, a scoring rule, or even a new
  JSON-shaping wasm-bindgen method), stop and make that change in `openlr-core`'s `openlr-wasm`,
  `openlr-engine`, or `openlr-encoder` instead, then rebuild the wasm module (see §1) to pick it up
  here.
- Rebuilding the wasm module after any `openlr-core` change is the one manual step this repo's own
  dev loop needs — `wasm-pack build` run from `openlr-core`'s `crates/openlr-wasm` (see §1 and the
  README's Build section), never a step inside this repo. Vite does not watch or rebuild it.
- This repo has no pipeline/tile-building code and no `fixtures/` corpus of its own — those live in
  `openlr-pmtiles`. Don't reintroduce `pipeline/`-shaped code or dependencies here; a tile-format
  change belongs there first, propagating to `openlr-core`'s decoder, never to this repo directly.
- When a decision is genuinely open, state the assumption inline and proceed; never silently
  violate a Critical Invariant (§2, or `openlr-core`'s own) to make something compile.
- **Docs and the onboarding tour drift silently — treat that as a bug, not cosmetic.** Nothing
  fails a build when `README.md`/`WebFrontend.md`, `web/src/docs/userGuide.md` (Documentation
  panel), or `web/src/components/OnboardingTour.jsx` fall out of sync with the actual UI/behavior —
  a stale tutorial or a tour spotlight pointing at nothing just looks like the tool doesn't work.
  Whenever a change touches user-facing behavior:
  - Adding/renaming/removing a menu item, panel, or decode/encode parameter → update
    `userGuideMd` and/or the `HELP` object in `refFormat.js` (the single source shared by the `?`
    tooltips and the Documentation panel's parameter reference — edit it once, not both places).
  - Renaming or removing a DOM element/class that carries a `data-tour` / `data-tour-solo`
    attribute, or restructuring where one lives → grep for that selector in
    `OnboardingTour.jsx`'s `STEPS` array first. `unionRect()` degrades silently on a no-match
    selector (empty spotlight, not an error), so a broken tour step won't surface on its own.
  - Architecture/setup changes (new crate, changed build/deploy steps, new invariant) → update
    the relevant doc in this repo (`README.md`, `WebFrontend.md`, or this file) — and check whether
    `openlr-core`'s own docs need the matching update too, if the change touches the boundary
    between the two repos.
