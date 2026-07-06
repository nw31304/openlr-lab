//! Canonical DuckDB ingestion schema — the interchange contract between an
//! external, format-specific producer (a SQL transform, or a program in any
//! language with DuckDB bindings) and this pipeline's low-memory ingest path.
//!
//! See `pipeline/schema/canonical_schema.sql` for the full spec and the
//! preconditions a producer must satisfy (pre-split, vehicular-only edges,
//! source-derived stable string IDs, WKT geometry).

use std::path::Path;

use anyhow::{Context, Result};
use duckdb::Connection;

/// Compile-time embed of the DDL so the binary never depends on the working
/// directory at runtime, and there is exactly one source of truth for the
/// schema shared between the published spec file and this binary.
pub const SCHEMA_SQL: &str = include_str!("../schema/canonical_schema.sql");

/// Create the two canonical tables in `conn` (`IF NOT EXISTS`, so this is
/// safe to call against a database a producer has already partially populated).
pub fn create_canonical_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL).context("create canonical schema")
}

/// Create a fresh DuckDB file at `path` with the canonical schema applied.
/// Refuses to overwrite an existing file — producers populate the returned
/// database themselves, so silently truncating one that already has data
/// would be a real footgun.
pub fn init_db(path: &Path) -> Result<()> {
    anyhow::ensure!(
        !path.exists(),
        "refusing to overwrite existing file: {} (remove it first if you want a fresh database)",
        path.display()
    );
    let conn = Connection::open(path)
        .with_context(|| format!("create DuckDB file {}", path.display()))?;
    create_canonical_schema(&conn)
        .with_context(|| format!("apply canonical schema to {}", path.display()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_sql_applies_cleanly_to_a_fresh_in_memory_db() {
        let conn = Connection::open_in_memory().unwrap();
        create_canonical_schema(&conn).unwrap();

        let table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.tables WHERE table_name LIKE 'canonical_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 2);
    }

    #[test]
    fn schema_sql_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_canonical_schema(&conn).unwrap();
        // Calling it again must not error (IF NOT EXISTS), matching the
        // "safe against a partially populated producer database" contract.
        create_canonical_schema(&conn).unwrap();
    }

    #[test]
    fn node_id_over_255_bytes_is_rejected() {
        // Node ids only ever appear on canonical_edges (start_node_id/
        // end_node_id) — there is no canonical_nodes table — so the byte-length
        // CHECK has to live there instead.
        let conn = Connection::open_in_memory().unwrap();
        create_canonical_schema(&conn).unwrap();
        let too_long = "x".repeat(256);
        let err = conn.execute(
            "INSERT INTO canonical_edges VALUES ('e1', ?, 'n2', 'LINESTRING (0 0, 1 1)', 3, 3, 'both')",
            duckdb::params![too_long],
        );
        assert!(err.is_err(), "256-byte node id must be rejected (tile format's stable_id_len is a u8)");

        let exactly_255 = "x".repeat(255);
        conn.execute(
            "INSERT INTO canonical_edges VALUES ('e1', ?, 'n2', 'LINESTRING (0 0, 1 1)', 3, 3, 'both')",
            duckdb::params![exactly_255],
        )
        .expect("255-byte node id must be accepted");
    }

    #[test]
    fn edges_do_not_require_a_pre_declared_node() {
        // The whole point of dropping canonical_nodes: an edge can reference
        // any node id string without it existing anywhere else first.
        let conn = Connection::open_in_memory().unwrap();
        create_canonical_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO canonical_edges VALUES ('e1', 'n1', 'n2', 'LINESTRING (0 0, 1 1)', 3, 3, 'both')",
            [],
        )
        .expect("edge must not require its endpoints to exist in a separate node table");
    }

    #[test]
    fn restriction_via_id_over_255_bytes_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        create_canonical_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO canonical_edges VALUES ('e1', 'n1', 'n2', 'LINESTRING (0 0, 1 1)', 3, 3, 'both')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO canonical_edges VALUES ('e2', 'n2', 'n3', 'LINESTRING (1 1, 2 2)', 3, 3, 'both')",
            [],
        )
        .unwrap();
        let too_long = "x".repeat(256);
        let err = conn.execute(
            "INSERT INTO canonical_restrictions (from_id, via_id, to_id) VALUES ('e1', ?, 'e2')",
            duckdb::params![too_long],
        );
        assert!(err.is_err(), "256-byte via_id must be rejected");
    }

    #[test]
    fn init_db_refuses_to_overwrite_existing_file() {
        let dir = std::env::temp_dir().join(format!("olrl-canon-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("existing.duckdb");
        std::fs::write(&path, b"not a real duckdb file").unwrap();

        let result = init_db(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
