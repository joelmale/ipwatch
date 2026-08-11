//! Numbered schema migrations, tracked with `PRAGMA user_version`.
//!
//! Append new migrations to `MIGRATIONS`; never edit one that has shipped.
//! `apply` runs everything above the stored version inside a transaction, so a
//! failure leaves the database at its previous version rather than half-migrated.

use rusqlite::Connection;

use super::DbError;

/// `(version, sql)` pairs, applied in ascending order.
const MIGRATIONS: &[(u32, &str)] = &[(
    1,
    r#"
    CREATE TABLE ip_events (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        ts            INTEGER NOT NULL,
        external_ip   TEXT    NOT NULL,
        country       TEXT,
        country_code  TEXT,
        isp           TEXT,
        change_reason TEXT    NOT NULL
    );
    CREATE INDEX idx_ip_events_ts ON ip_events(ts DESC);
    "#,
)];

/// The schema version this build expects.
pub fn target_version() -> u32 {
    MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0)
}

fn current_version(conn: &Connection) -> Result<u32, DbError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as u32)
}

/// Brings the database up to `target_version()`. A no-op when already current.
pub fn apply(conn: &mut Connection) -> Result<(), DbError> {
    let from = current_version(conn)?;

    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > from) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql).map_err(|e| DbError::Migration {
            version: *version,
            detail: e.to_string(),
        })?;
        // PRAGMA does not accept bound parameters.
        tx.execute_batch(&format!("PRAGMA user_version = {version}"))
            .map_err(|e| DbError::Migration {
                version: *version,
                detail: e.to_string(),
            })?;
        tx.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_from_empty_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();

        apply(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), target_version());

        // Re-running must not error or re-execute (CREATE TABLE would fail).
        apply(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), target_version());
    }

    #[test]
    fn creates_ip_events_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='ip_events'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
