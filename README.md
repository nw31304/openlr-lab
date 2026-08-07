# OpenLRLab

A browser-based diagnostic decoder **and encoder** for [OpenLR](https://www.openlr.org/) location references. This repo contains no Rust code — the decode/encode engine (codec, graph, A\* path search, encoder) *and* the `wasm-bindgen` binding crate that compiles it to WebAssembly both live in a separate repo, [openlr-core](https://github.com/nw31304/openlr-core); this repo builds that crate and copies the resulting wasm module in. A MapLibre GL JS front end renders the decoded/encoded path and step-by-step diagnostics.

Two formats are supported, both for decode and encode:

- **OpenLR binary v3** (TomTom) — 11.25° bearing buckets, ~58.6 m DNP buckets
- **TPEG-OLR / ISO 21219-22** — full-precision intervals

**Read [`CLAUDE.md`](CLAUDE.md) before writing any code.** It's the canonical reference for this
repo's critical invariants, architecture, data model, and agent conventions — several of those
invariants fail *silently* (wrong output, not a crash) if violated. This README covers build/run
instructions; `CLAUDE.md` covers everything about *why* the code is shaped the way it is.

## Architecture

```
BUILD TIME  (a few times per year, separate repo: openlr-pmtiles)
  Road network source data ──▶ openlr-pmtiles-build ──▶ PMTiles archive ──▶ R2 / CDN

RUNTIME  (browser, no server)
  PMTiles (range reads) ──▶ TileLoader ──▶ OpenLRDataProvider ──▶ in-memory graph   ┐
                                                    │                                │ openlr-core
  decode:  OpenLR string ──▶ codec (v3 / TPEG) ──▶ unified LRP model                │ (separate repo —
                                                    │                                │  engine AND the
                             engine: candidate selection + A* + validation          │  openlr-wasm
                                                    │                                │  bindings crate
  encode:  waypoints (map clicks) ──▶ snap + route ──▶ encoder (Rule-1/Rule-4)      │  both live there)
                                                    │                                │
                             codec (v3 / TPEG) ──▶ round-trip verify (decode)       │
                                                    │                                │
                             openlr-wasm (openlr-core) ────────────────────────────┘
                                                    │
                              compiled wasm module, copied into web/src/wasm
                                                    │
                                        diagnostics + MapLibre UI  (this repo)
```

All map I/O stays in JavaScript. WASM receives pre-fetched tile bytes and operates synchronously over an in-memory cache, avoiding async-trait across the FFI boundary.

### Crates

Every crate below lives in [openlr-core](https://github.com/nw31304/openlr-core) — this repo has no `Cargo.toml`/`crates/` of its own.

| Crate | Role |
|---|---|
| `openlr-codec` | v3 / TPEG-OLR binary parsing and serialization ↔ unified `Lrp` model |
| `openlr-graph` | Tile format, segment/node tables, geometry pool |
| `openlr-decoder` | Decode: candidate selection, A\* (`state = (node, incoming_segment)`), scoring, diagnostics |
| `openlr-encoder` | Encode: Rule-1/Rule-4 Line and PointAlongLine encoding, boundary expansion, coverage sweep, waypoint snapping |
| `openlr-provider` | `OpenLRDataProvider` trait + `PmtilesProvider` implementation |
| `openlr-cli` | Native batch-decode binary against a local `.pmtiles` archive |
| `openlr-wasm` | Thin `wasm-bindgen` adapter exposing `Decoder`/`Encoder` to JS — JSON shaping and the tile-injection protocol only, no algorithmic logic. This is what this repo builds and copies in (see Build, below). |

Clone `openlr-core` alongside this repo (`../openlr-core` relative to here) before building — see
`CLAUDE.md` §1 for why building its `openlr-wasm` crate from a sibling checkout, rather than
vendoring Rust source into this repo, was chosen.

The PMTiles builder (`openlr-pmtiles-build`, ingesting Overture, OSM, generic
GeoJSONL, or a canonical DuckDB source) lives in a separate repo,
[openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles) — `openlr-core` (not this repo) is the
consumer of the archives it produces. Only the tile **format** (magic, header layout,
segment/node/restriction records) is a contract shared between those two repos; a format change
must land in openlr-pmtiles first, then propagate to `openlr-core`'s `openlr-provider` decoder.

### Web frontend

Vite + React + MapLibre GL JS + Zustand. Source lives in `web/`.

## Diagnostics

The UI is a stepped debugger, not just a result renderer:

- **Candidate panel** — per-LRP candidate table with bearing wedge, DNP band, and per-term scores. Each candidate shows whether it snapped to an interior point, start endpoint, or end endpoint.
- **A\* replay** — step-forward/backward through the search frontier.
- **Forced-decode mode** — pin any candidate per LRP and re-run A\* to see why the encoder's intended path was accepted or rejected.
- **Encode mode** — draw waypoints directly on the map (click to append, drag to insert/move) for a Line or PointAlongLine location; a live route preview snaps and routes between them as you go. Confirming the last waypoint automatically encodes to both binary v3 and TPEG-OLR and immediately decodes the result back, so every encode is round-trip-verified against the exact same engine a consumer would use.
- **LLM chat** — optional AI assistant with full access to the decode trace, encode/verify state, candidate scores, and graph geometry. Bring your own key (OpenAI / Anthropic).

## Prerequisites

- Rust toolchain + `wasm-pack` (needed to build `openlr-core`'s `openlr-wasm` crate; this repo
  itself has no Rust code)
- Node.js ≥ 18
- [openlr-core](https://github.com/nw31304/openlr-core) cloned as a sibling directory of this repo
  (`../openlr-core`) — owns the decode/encode engine *and* the `openlr-wasm` binding crate this
  repo builds. See `CLAUDE.md` §1.

## Build

### 1. Compile the WASM module

```sh
cd ../openlr-core/crates/openlr-wasm
wasm-pack build --target web --out-dir ../../../openlr-lab/web/src/wasm
```

(Run from this repo's own root; adjust the relative path if your sibling checkout lives elsewhere.)

**This is not a one-time step.** The Vite dev server does not watch or rebuild the WASM module —
re-run this command and reload the browser after *every* change in the sibling `openlr-core`
checkout, or you'll silently keep testing against a stale binary. The output (`web/src/wasm/`) is
gitignored, so `npm run deploy` (see Deployment, below) also rebuilds it fresh before every deploy.

### 2. Run the web dev server

```sh
cd web
npm install
npm run dev
```

`npm run dev` starts both the Vite dev server (default `localhost:5173`) and a built-in tile server at `http://localhost:5176` (see the `tile-server` plugin in `vite.config.js`). By default it serves range requests out of the path hardcoded as `DEFAULT_TILES_DIR` in `vite.config.js`; set `OPENLR_TILES_DIR` to point it at wherever [openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles) built its archives instead (e.g. `OPENLR_TILES_DIR=../../openlr-pmtiles/out npm run dev`). Override the tile source in the **Tile source** menu if you're pointing at a different archive or host.

Alternatively, `web/scripts/dev.sh start`/`stop` wraps this step — checks for (and offers to kill)
anything already listening on the webapp/tile-server ports, backgrounds `npm run dev`, and waits
until it actually responds before returning. Run `web/scripts/dev.sh --help` for its flags
(`--port`, `--tiles-dir`).

### 3. Get a tile archive

The app can't do much without one — no PMTiles archive means the map loads with no road data and
decode/encode has nothing to snap against. Building one is a separate repo:
[openlr-pmtiles](https://github.com/nw31304/openlr-pmtiles) (private). See its README for build
commands. Point this repo's dev server at its output via `OPENLR_TILES_DIR` (step 2), or serve the
archive from any PMTiles-compatible host (e.g. [`pmtiles serve`](https://github.com/protomaps/go-pmtiles),
or R2/CDN with range-request support) and point the app at it via the **Tile source** menu. There is
currently no sample/test archive bundled in this repo and no public fallback host documented — if
you don't have access to the `openlr-pmtiles` repo or an existing archive, you can compile and run
the app (steps 1–2) but won't see live map data.

## Deployment

Production hosting is Cloudflare Pages for the static SPA, with the `world.pmtiles` archive served from an R2 bucket via a same-origin Pages Function (`web/functions/tiles/[[path]].js`) rather than exposed directly — no CI, built and deployed locally with `wrangler`. Full details (one-time setup, R2 binding, env vars) are in `WebFrontend.md` §21.

**One-time setup:**
```sh
npx wrangler login
npx wrangler r2 bucket create openlr-lab-tiles       # enable R2 in the dashboard first, once per account
npx wrangler pages project create openlr-lab --production-branch=main
```

**Push a tile archive to R2** (whenever `openlr-pmtiles` finishes a fresh build). `world.pmtiles` is
multi-GB, well past `wrangler r2 object put`'s 300MB cap, so it goes up via `rclone` (S3-compatible)
instead — `manifest.json` is tiny and still just uses `wrangler`:
```sh
rclone copyto /path/to/world.pmtiles r2:openlr-lab-tiles/world.pmtiles --s3-no-check-bucket -P
wrangler r2 object put openlr-lab-tiles/manifest.json --file=/path/to/manifest.json
```
One-time `rclone` remote setup and the `--s3-no-check-bucket` rationale: see `WebFrontend.md` §21.

**Day-to-day deploy:**
```sh
cd web && npm run deploy
```
This builds the WASM module fresh (it's gitignored, never committed), runs `vite build`, then `wrangler pages deploy dist --project-name=openlr-lab`.

## Tile format

Custom binary payload (magic `OLRL`, version 3). All integers little-endian, single zoom level (default z12). Segments are post-split at every interior junction — junctions are never elided. Each segment and node carries a provider-defined opaque stable ID (UTF-8 string, stored in a per-tile string pool). The full byte-level layout is owned by [openlr-core](https://github.com/nw31304/openlr-core) now — see that repo's `CLAUDE.md §4–5`.

## License

Web frontend: MIT. Derived tile data license depends on the source data used to build it: OSM-derived sources (OSM directly, or any provider whose road-network theme is OSM-derived, e.g. Overture) carry **ODbL** — any served output must preserve attribution and honour share-alike obligations.
