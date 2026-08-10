//! SQLite persistence for the IP change log.
//!
//! `Db::open` takes a plain path rather than a Tauri handle so tests can pass
//! `":memory:"` and stay independent of the app runtime. The real database
//! lives in `app_data_dir()`; that path is chosen by the caller.

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub mod migrations;

/// Why a snapshot was recorded. `Initial` is the first observation of a
/// session; `Country`/`Isp` changes are the VPN-drop signals.
///
/// `rename_all = "snake_case"` keeps the wire form (sent to the frontend and
/// used by `serde_json`) identical to `as_str()`'s DB representation — see
/// `serde_matches_as_str` below, which pins that down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeReason {
    Initial,
    IpChanged,
    CountryChanged,
    IspChanged,
    Offline,
}

impl ChangeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::IpChanged => "ip_changed",
            Self::CountryChanged => "country_changed",
            Self::IspChanged => "isp_changed",
            Self::Offline => "offline",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "initial" => Self::Initial,
            "ip_changed" => Self::IpChanged,
            "country_changed" => Self::CountryChanged,
            "isp_changed" => Self::IspChanged,
            "offline" => Self::Offline,
            _ => return None,
        })
    }
}

/// One row of `ip_events`. `id` is `None` before insertion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpEvent {
    pub id: Option<i64>,
    /// Unix seconds.
    pub ts: i64,
    pub external_ip: String,
    pub country: Option<String>,
    pub country_code: Option<String>,
    pub isp: Option<String>,
    pub change_reason: ChangeReason,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("migration {version} failed: {detail}")]
    Migration { version: u32, detail: String },
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (creating if needed) and brings the schema up to date.
    /// Pass `":memory:"` for an ephemeral database.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        let conn = if path.as_ref() == Path::new(":memory:") {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Applies any migrations newer than `PRAGMA user_version`. Idempotent.
    fn migrate(&mut self) -> Result<(), DbError> {
        migrations::apply(&mut self.conn)
    }

    pub fn insert_event(&self, event: &IpEvent) -> Result<i64, DbError> {
        self.conn.execute(
            "INSERT INTO ip_events (ts, external_ip, country, country_code, isp, change_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                event.ts,
                event.external_ip,
                event.country,
                event.country_code,
                event.isp,
                event.change_reason.as_str(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Most recent events first.
    pub fn recent_events(&self, limit: u32) -> Result<Vec<IpEvent>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, external_ip, country, country_code, isp, change_reason
             FROM ip_events ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            Ok(IpEvent {
                id: Some(row.get(0)?),
                ts: row.get(1)?,
                external_ip: row.get(2)?,
                country: row.get(3)?,
                country_code: row.get(4)?,
                isp: row.get(5)?,
                change_reason: row
                    .get::<_, String>(6)
                    .map(|s| ChangeReason::from_str(&s).unwrap_or(ChangeReason::Initial))?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn latest_event(&self) -> Result<Option<IpEvent>, DbError> {
        Ok(self.recent_events(1)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(ts: i64, ip: &str, reason: ChangeReason) -> IpEvent {
        IpEvent {
            id: None,
            ts,
            external_ip: ip.to_string(),
            country: Some("United States".into()),
            country_code: Some("US".into()),
            isp: Some("Example ISP".into()),
            change_reason: reason,
        }
    }

    fn db() -> Db {
        Db::open(":memory:").expect("in-memory db opens")
    }

    #[test]
    fn round_trips_an_event() {
        let db = db();
        let original = event(1_700_000_000, "203.0.113.7", ChangeReason::Initial);

        let id = db.insert_event(&original).unwrap();
        let stored = db.latest_event().unwrap().expect("one row");

        assert_eq!(stored.id, Some(id));
        assert_eq!(stored.ts, original.ts);
        assert_eq!(stored.external_ip, original.external_ip);
        assert_eq!(stored.country, original.country);
        assert_eq!(stored.country_code, original.country_code);
        assert_eq!(stored.isp, original.isp);
        assert_eq!(stored.change_reason, ChangeReason::Initial);
    }

    #[test]
    fn recent_events_returns_newest_first() {
        let db = db();
        db.insert_event(&event(100, "198.51.100.1", ChangeReason::Initial)).unwrap();
        db.insert_event(&event(300, "198.51.100.3", ChangeReason::CountryChanged)).unwrap();
        db.insert_event(&event(200, "198.51.100.2", ChangeReason::IpChanged)).unwrap();

        let rows = db.recent_events(10).unwrap();
        let timestamps: Vec<i64> = rows.iter().map(|e| e.ts).collect();

        assert_eq!(timestamps, vec![300, 200, 100]);
    }

    #[test]
    fn recent_events_honours_the_limit() {
        let db = db();
        for i in 0..5 {
            db.insert_event(&event(i, "203.0.113.7", ChangeReason::IpChanged)).unwrap();
        }

        assert_eq!(db.recent_events(2).unwrap().len(), 2);
    }

    #[test]
    fn latest_event_is_none_on_an_empty_database() {
        assert!(db().latest_event().unwrap().is_none());
    }

    #[test]
    fn nullable_geo_fields_survive_the_round_trip() {
        // The free API tiers omit fields unpredictably, so NULLs are routine.
        let db = db();
        let sparse = IpEvent {
            id: None,
            ts: 42,
            external_ip: "203.0.113.7".into(),
            country: None,
            country_code: None,
            isp: None,
            change_reason: ChangeReason::Offline,
        };

        db.insert_event(&sparse).unwrap();
        let stored = db.latest_event().unwrap().unwrap();

        assert_eq!(stored.country, None);
        assert_eq!(stored.country_code, None);
        assert_eq!(stored.isp, None);
        assert_eq!(stored.change_reason, ChangeReason::Offline);
    }

    #[test]
    fn every_change_reason_survives_the_string_round_trip() {
        // as_str/from_str are the persistence boundary; a mismatch would
        // silently degrade stored history to "initial" on read.
        for reason in [
            ChangeReason::Initial,
            ChangeReason::IpChanged,
            ChangeReason::CountryChanged,
            ChangeReason::IspChanged,
            ChangeReason::Offline,
        ] {
            assert_eq!(ChangeReason::from_str(reason.as_str()), Some(reason));
        }
    }

    #[test]
    fn serde_matches_as_str() {
        // The frontend and the DB's stored strings must agree exactly, or
        // history rows serialized to JSON would desync from what `as_str()`
        // persists. Pin every variant down, not just one.
        for (reason, expected) in [
            (ChangeReason::Initial, "initial"),
            (ChangeReason::IpChanged, "ip_changed"),
            (ChangeReason::CountryChanged, "country_changed"),
            (ChangeReason::IspChanged, "isp_changed"),
            (ChangeReason::Offline, "offline"),
        ] {
            assert_eq!(reason.as_str(), expected);
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::String(expected.to_string())
            );
        }
    }

    #[test]
    fn reopening_an_existing_database_is_safe() {
        // Guards the startup path: migrations must be a no-op on an existing file.
        let dir = std::env::temp_dir().join("ipwatch-db-reopen-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ipwatch.db");

        {
            let db = Db::open(&path).unwrap();
            db.insert_event(&event(1, "203.0.113.7", ChangeReason::Initial)).unwrap();
        }

        let reopened = Db::open(&path).unwrap();
        assert_eq!(reopened.recent_events(10).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
