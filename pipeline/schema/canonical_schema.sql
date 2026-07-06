-- OpenLRLens canonical ingestion schema.
--
-- This is the interchange contract between a format-specific producer (a SQL
-- transform, or a program in any language with DuckDB bindings) and the
-- openlr-lens pipeline binary. A producer populates these two tables in a
-- DuckDB database file; the pipeline's "ingest from existing DuckDB" mode
-- reads them directly and runs the same split/quantize/tile logic used by
-- every other source (OSM, Overture, generic GeoJSONL).
--
-- Preconditions a producer MUST satisfy before writing rows:
--   * The road network is already split at every interior junction — one row
--     in canonical_edges per final node-to-node graph edge, not per raw
--     source way/segment that may span multiple junctions. (Sources that
--     cannot guarantee this, like raw OSM ways, are handled by the
--     pipeline's own native importers instead of this path.)
--   * Only vehicular, routable segments are present — non-vehicular segments
--     (footpaths, cycleways, etc.) must already be filtered out.
--   * Both tables must exist, even if canonical_restrictions is empty.
--   * Every edge sharing a given node id agrees on that node's coordinate
--     (its own geometry's first/last vertex) — the pipeline takes whichever
--     edge it encounters first as that node's coordinate and does not
--     cross-check the rest.
--
-- IDs are opaque UTF-8 strings, never surrogate/sequential integers (see
-- CLAUDE.md Invariant 2: stable IDs must be deterministic and derived from
-- the source data, never build/row order). Use whatever the source format's
-- own persistent identifier is — an OSM node/way id, a MultiNet-R FEAT_ID, a
-- database primary key — as long as it is stable across rebuilds of the same
-- source data. IDs are stored in the tile's string pool with a 1-byte length
-- prefix, so every id MUST be at most 255 bytes (UTF-8 byte length, not
-- character count) — enforced below.

-- There is deliberately no canonical_nodes table. A node's coordinate is
-- already implied by whichever edge's geometry touches it (see the geometry
-- column below), so a separate node table would just be a second place for a
-- producer to state the same coordinate and risk it disagreeing with the
-- first. Node *identity* (start_node_id/end_node_id/via_id) is carried
-- entirely as opaque strings on canonical_edges/canonical_restrictions and
-- becomes the tile's stable_id for that node; the pipeline derives each
-- node's coordinate from the first edge endpoint it encounters that
-- references it.

CREATE TABLE IF NOT EXISTS canonical_edges (
    -- Persistent, source-defined identifier for this final (already-split)
    -- edge. Becomes the tile's stable_id for this segment.
    id             TEXT NOT NULL PRIMARY KEY CHECK (octet_length(encode(id)) BETWEEN 1 AND 255),

    -- Persistent, source-defined node identifiers — NOT surrogate integers.
    -- Not validated against a node table (there isn't one); the pipeline
    -- treats a node id as real once it has seen it as some edge's endpoint.
    start_node_id  TEXT NOT NULL CHECK (octet_length(encode(start_node_id)) BETWEEN 1 AND 255),
    end_node_id    TEXT NOT NULL CHECK (octet_length(encode(end_node_id)) BETWEEN 1 AND 255),

    -- WGS84 LineString as WKT text, e.g. "LINESTRING (lon lat, lon lat, ...)".
    -- At least 2 points. First point must equal the start node's coordinate,
    -- last point must equal the end node's coordinate (within source
    -- precision) — the pipeline does not re-snap geometry to node coords.
    -- Full fidelity only: no simplification beyond exact-collinear removal,
    -- which the pipeline itself performs downstream (Invariant 4).
    geometry       TEXT NOT NULL,

    -- OpenLR Functional Road Class, 0 (Motorway) .. 7 (Other/Local).
    frc            UTINYINT NOT NULL CHECK (frc BETWEEN 0 AND 7),

    -- OpenLR Form Of Way: 0=Undefined 1=Motorway 2=MultipleCarriageway
    -- 3=SingleCarriageway 4=Roundabout 5=TrafficSquare 6=SlipRoad 7=Other.
    fow            UTINYINT NOT NULL CHECK (fow BETWEEN 0 AND 7),

    -- Direction of legal travel relative to the geometry's own vertex order
    -- (start_node_id -> end_node_id is 'fwd').
    direction      TEXT NOT NULL CHECK (direction IN ('fwd', 'rev', 'both'))

    -- No length column: the pipeline always computes edge length itself from
    -- `geometry`, for consistency with every other source rather than trusting
    -- a producer-supplied value that may use a different great-circle model.
);

CREATE TABLE IF NOT EXISTS canonical_restrictions (
    -- A turn restriction: travel from `from_id` through node `via_id` onto
    -- `to_id` is prohibited. from_id/to_id must resolve into canonical_edges.id;
    -- via_id must equal a node id shared by both edges (from_id's end_node_id /
    -- to_id's start_node_id in the common case) — not checked at the schema
    -- level (there is no node table to check against), but the pipeline drops
    -- any restriction where via_id doesn't actually connect from_id to to_id.
    from_id       TEXT NOT NULL REFERENCES canonical_edges(id),
    via_id        TEXT NOT NULL CHECK (octet_length(encode(via_id)) BETWEEN 1 AND 255),
    to_id         TEXT NOT NULL REFERENCES canonical_edges(id),

    -- Optional direction constraints, using the same vocabulary as
    -- canonical_edges.direction. 'both' (default) means the restriction
    -- applies regardless of which direction the segment is traversed —
    -- correct for one-way segments and the common case.
    from_heading  TEXT NOT NULL DEFAULT 'both' CHECK (from_heading IN ('fwd', 'rev', 'both')),
    to_heading    TEXT NOT NULL DEFAULT 'both' CHECK (to_heading   IN ('fwd', 'rev', 'both'))
);
