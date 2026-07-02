# OpenLRLens

A browser-based diagnostic decoder for [OpenLR](https://www.openlr-association.com/) location references. The Rust core compiles to WebAssembly and runs the full codec, graph, and A\* path search entirely client-side. A MapLibre GL JS front end renders the decoded path and step-by-step diagnostics.

Two decode formats are supported:

- **OpenLR binary v3** (TomTom) — 11.25° bearing buckets, ~58.6 m DNP buckets
- **TPEG-OLR / ISO 21219-22** — full-precision intervals

## Architecture

```
BUILD TIME  (a few times per year)
  Overture / OSM road network ──▶ pipeline/ ──▶ PMTiles archive ──▶ R2 / CDN

RUNTIME  (browser, no server)
  PMTiles (range reads) ──▶ TileLoader ──▶ OpenLRDataProvider ──▶ in-memory graph
                                                    │
  OpenLR string ──▶ codec (v3 / TPEG) ──▶ unified LRP model
                                                    │
                                  engine: candidate selection + A* + validation
                                                    │
                                         diagnostics + MapLibre UI
```

All map I/O stays in JavaScript. WASM receives pre-fetched tile bytes and operates synchronously over an in-memory cache, avoiding async-trait across the FFI boundary.

### Rust crates

| Crate | Role |
|---|---|
| `openlr-codec` | v3 / TPEG-OLR binary parsing → unified `Lrp` model |
| `openlr-graph` | Tile format, segment/node tables, geometry pool |
| `openlr-engine` | Candidate selection, A\* (`state = (node, incoming_segment)`), scoring, diagnostics |
| `openlr-provider` | `OpenLRDataProvider` trait + `PmtilesProvider` implementation |
| `openlr-wasm` | `wasm-bindgen` glue exposing `decode` / `decode_forced` to JS |
| `pipeline` | One-shot CLI to build a PMTiles archive from Overture or generic GeoJSONL |

### Web frontend

Vite + React + MapLibre GL JS + Zustand. Source lives in `web/`.

## Diagnostics

The UI is a stepped debugger, not just a result renderer:

- **Candidate panel** — per-LRP candidate table with bearing wedge, DNP band, and per-term scores. Each candidate shows whether it snapped to an interior point, start endpoint, or end endpoint.
- **A\* replay** — step-forward/backward through the search frontier.
- **Forced-decode mode** — pin any candidate per LRP and re-run A\* to see why the encoder's intended path was accepted or rejected.
- **LLM chat** — optional AI assistant with full access to the decode trace, candidate scores, and graph geometry. Bring your own key (OpenAI / Anthropic).

## Prerequisites

- Rust toolchain + `wasm-pack`
- Node.js ≥ 18

## Build

### 1. Compile the WASM module

```sh
cd crates/openlr-wasm
wasm-pack build --target web --out-dir ../../web/src/wasm
```

### 2. Run the web dev server

```sh
cd web
npm install
npm run dev
```

The app expects a tile server at `http://localhost:5176` by default. You can override this in the **Tile source** menu.

### 3. Build a tile archive (optional — if you have road network data)

```sh
# List available Overture releases
cargo run -p pipeline -- list-releases

# Build a PMTiles archive for a bounding box
cargo run -p pipeline -- build \
  --release 2024-07-22.0 \
  --extent 4.7,52.2,5.1,52.5 \
  --output amsterdam.pmtiles

# Or build from generic GeoJSONL (any source with frc/fow/flowdir attributes)
cargo run -p pipeline -- build \
  --geojsonl roads.geojsonl.gz \
  --output roads.pmtiles

# Merge regional archives into one
cargo run -p pipeline -- merge region-a.pmtiles region-b.pmtiles --output combined.pmtiles
```

Serve the resulting `.pmtiles` file with any PMTiles-compatible tile server (e.g. [`pmtiles serve`](https://github.com/protomaps/go-pmtiles)) and point the app at it.

## Tile format

Custom binary payload (magic `OLRL`, version 1). All integers little-endian, single zoom level (default z12). Segments are post-split at every interior connector — junctions are never elided. See `CLAUDE.md §4–5` for the full layout.

## License

Web frontend: MIT. Derived tile data: **ODbL** (OSM-derived via Overture). Any served output must preserve OSM + Overture attribution and honour share-alike obligations.
