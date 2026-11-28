//! SQLite storage for the calendar plugin.
//!
//! Replaces the former whole-file rewrite of `calendar.json`. All calendar
//! events live in `calendar.db` alongside the Google-sync tables
//! (`sync_config`, `sync_state`, `push_queue`); secrets never go here —
//! OAuth tokens stay in the OS keychain (see `google_oauth.rs`).
//!
//! The in-memory `Vec<CalendarEvent>` cache in `lib.rs` stays the working
//! set (scheduler tick, list output); every mutation site writes through
//! with targeted upserts/deletes instead of rewriting a file.

use crate::{CalendarEvent, Repeat};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub const DB_FILE: &str = "calendar.db";
const LEGACY_JSON: &str = "calendar.json";
const LEGACY_JSON_BAK: &str = "calendar.json.bak";

/// A queued push operation. `payload` carries the Google ids captured when
/// the local event was deleted (they are gone from the store by the time the
/// worker drains the queue).
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub seq: i64,
    pub local_id: String,
    /// "upsert" | "delete"
    pub op: String,
    /// JSON `{"calendar_id": ..., "event_id": ...}` (delete ops; absent when
    /// the event was never pushed).
    pub payload: Option<String>,
    pub attempts: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Store, String> {
        let path = data_dir.join(DB_FILE);
        let conn = Connection::open(&path).map_err(|e| format!("open calendar.db: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                description TEXT NOT NULL DEFAULT '',
                date TEXT NOT NULL DEFAULT '',
                hour INTEGER NOT NULL DEFAULT 9,
                minute INTEGER NOT NULL DEFAULT 0,
                all_day INTEGER NOT NULL DEFAULT 0,
                end_hour INTEGER,
                end_minute INTEGER,
                repeat TEXT,
                message TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                last_fired INTEGER,
                source TEXT,
                google_calendar_id TEXT,
                google_event_id TEXT,
                updated_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_events_google
                ON events(google_calendar_id, google_event_id);
            CREATE INDEX IF NOT EXISTS idx_events_date ON events(date);
            CREATE TABLE IF NOT EXISTS sync_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sync_state (
                calendar_id TEXT PRIMARY KEY,
                sync_token TEXT,
                last_sync INTEGER
            );
            CREATE TABLE IF NOT EXISTS push_queue (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                local_id TEXT NOT NULL,
                op TEXT NOT NULL,
                payload TEXT,
                attempts INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| format!("init calendar.db schema: {e}"))?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    // -- events --------------------------------------------------------------

    /// Import a legacy `calendar.json` on first run with an empty DB. The
    /// file is renamed to `calendar.json.bak` (one-way migration).
    pub fn migrate_from_json(&self, data_dir: &Path) -> Result<usize, String> {
        let legacy = data_dir.join(LEGACY_JSON);
        if self.count_events() > 0 || !legacy.exists() {
            return Ok(0);
        }
        let raw = std::fs::read_to_string(&legacy)
            .map_err(|e| format!("read legacy {LEGACY_JSON}: {e}"))?;
        let list: Vec<CalendarEvent> = serde_json::from_str::<Vec<CalendarEvent>>(&raw)
            .unwrap_or_default()
            .into_iter()
            .map(|mut e| {
                // The schema has no repeat_yearly column: normalize legacy
                // yearly-only events into the repeat rule on the way in.
                if e.repeat.is_none() && e.repeat_yearly {
                    e.repeat = Some(Repeat::Yearly);
                }
                e
            })
            .collect();
        self.upsert_events(&list)?;
        let n = list.len();
        match std::fs::rename(&legacy, data_dir.join(LEGACY_JSON_BAK)) {
            Ok(()) => {}
            Err(e) => {
                // Keep going: the data is already imported; the stale file
                // would just re-suggest migration on an empty DB.
                eprintln!("[calendar] rename legacy json failed: {e}");
            }
        }
        Ok(n)
    }

    pub fn count_events(&self) -> i64 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn load_events(&self) -> Vec<CalendarEvent> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare(
            "SELECT id, title, description, date, hour, minute, all_day, end_hour, end_minute,
                    repeat, message, enabled, last_fired, source, google_calendar_id,
                    google_event_id, updated_at
             FROM events",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], row_to_event)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn upsert_event(&self, e: &CalendarEvent) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        upsert_one(&conn, e)
    }

    pub fn upsert_events(&self, list: &[CalendarEvent]) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        for e in list {
            upsert_one(&conn, e)?;
        }
        Ok(())
    }

    pub fn delete_event(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM events WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Delete every local row synced from one Google event — used when
    /// Google cancels it or the local mapping is replaced.
    pub fn delete_google_rows(&self, calendar_id: &str, google_event_id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM events WHERE google_calendar_id = ?1 AND google_event_id = ?2",
            params![calendar_id, google_event_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// One-time row-model migration to the unified Google mapping (v2):
    /// older versions expanded complex recurring series into per-occurrence
    /// instance rows (`gcal-<cal>-<gid>-<date>`). Drop those and clear the
    /// sync tokens so the next cycle full-pulls each calendar and rewrites
    /// every Google event as a single editable row.
    pub fn migrate_unified_rows(&self) -> Result<(), String> {
        const VERSION_KEY: &str = "row_model_version";
        const CURRENT: &str = "2";
        if self.config_get(VERSION_KEY).as_deref() == Some(CURRENT) {
            return Ok(());
        }
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM events WHERE source = 'google' AND id LIKE 'gcal-%-20__-__-__'",
            [],
        )
        .map_err(|e| format!("drop expanded instances: {e}"))?;
        drop(conn);
        self.sync_state_clear()?;
        self.config_set(VERSION_KEY, CURRENT)
    }

    /// `op_toggle` write-through: enabled flag + optional last_fired reset.
    pub fn set_enabled(&self, id: &str, enabled: bool, reset_last_fired: bool) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        if reset_last_fired {
            conn.execute(
                "UPDATE events SET enabled = ?1, last_fired = NULL WHERE id = ?2",
                params![enabled as i64, id],
            )
            .map_err(|e| e.to_string())?;
        } else {
            conn.execute(
                "UPDATE events SET enabled = ?1 WHERE id = ?2",
                params![enabled as i64, id],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Write the Google ids back to an app-created event after a successful
    /// push insert.
    pub fn set_google_ids(
        &self,
        local_id: &str,
        calendar_id: &str,
        google_event_id: &str,
        updated_at: i64,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE events SET google_calendar_id = ?1, google_event_id = ?2, updated_at = ?3
             WHERE id = ?4",
            params![calendar_id, google_event_id, updated_at, local_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // -- sync_config (non-secret settings) ------------------------------------

    pub fn config_get(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT value FROM sync_config WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()
        .unwrap_or(None)
    }

    pub fn config_set(&self, key: &str, value: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO sync_config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn config_delete(&self, key: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM sync_config WHERE key = ?1", params![key])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // -- sync_state (per-calendar incremental sync tokens) --------------------

    pub fn sync_token_get(&self, calendar_id: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT sync_token FROM sync_state WHERE calendar_id = ?1",
            params![calendar_id],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None)
        .flatten()
    }

    pub fn sync_state_set(&self, calendar_id: &str, sync_token: Option<&str>) -> Result<(), String> {
        let now = now_millis();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO sync_state (calendar_id, sync_token, last_sync) VALUES (?1, ?2, ?3)
             ON CONFLICT(calendar_id) DO UPDATE SET
                sync_token = excluded.sync_token, last_sync = excluded.last_sync",
            params![calendar_id, sync_token, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Most recent successful sync across all calendars (for the UI).
    pub fn last_sync(&self) -> Option<i64> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT MAX(last_sync) FROM sync_state", [], |r| r.get(0))
            .optional()
            .unwrap_or(None)
            .flatten()
    }

    pub fn sync_state_clear(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM sync_state", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // -- push_queue -------------------------------------------------------------

    pub fn queue_add(&self, local_id: &str, op: &str, payload: Option<&str>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO push_queue (local_id, op, payload, attempts, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![local_id, op, payload, now_millis()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Oldest queued entry (FIFO).
    pub fn queue_next(&self) -> Option<QueueEntry> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT seq, local_id, op, payload, attempts FROM push_queue ORDER BY seq LIMIT 1",
            [],
            |r| {
                Ok(QueueEntry {
                    seq: r.get(0)?,
                    local_id: r.get(1)?,
                    op: r.get(2)?,
                    payload: r.get(3)?,
                    attempts: r.get(4)?,
                })
            },
        )
        .optional()
        .unwrap_or(None)
    }

    pub fn queue_remove(&self, seq: i64) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM push_queue WHERE seq = ?1", params![seq])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn queue_touch(&self, seq: i64) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE push_queue SET attempts = attempts + 1 WHERE seq = ?1",
            params![seq],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drop a superseded entry (e.g. an upsert whose event was then deleted).
    pub fn queue_remove_for_local(&self, local_id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM push_queue WHERE local_id = ?1", params![local_id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn queue_count(&self) -> i64 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row("SELECT COUNT(*) FROM push_queue", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Local ids with a pending "upsert" — the pull pass must not clobber
    /// those events' Google-owned fields (the newer local push wins).
    pub fn queue_pending_upsert_ids(&self) -> std::collections::HashSet<String> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Default::default(),
        };
        let mut stmt = match conn.prepare("SELECT local_id FROM push_queue WHERE op = 'upsert'") {
            Ok(s) => s,
            Err(_) => return Default::default(),
        };
        stmt.query_map([], |r| r.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn queue_clear(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM push_queue", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn upsert_one(conn: &Connection, e: &CalendarEvent) -> Result<(), String> {
    conn.execute(
        "INSERT INTO events (id, title, description, date, hour, minute, all_day,
                end_hour, end_minute, repeat, message, enabled, last_fired,
                source, google_calendar_id, google_event_id, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT(id) DO UPDATE SET
            title = excluded.title, description = excluded.description,
            date = excluded.date, hour = excluded.hour, minute = excluded.minute,
            all_day = excluded.all_day, end_hour = excluded.end_hour,
            end_minute = excluded.end_minute, repeat = excluded.repeat,
            message = excluded.message, enabled = excluded.enabled,
            last_fired = excluded.last_fired, source = excluded.source,
            google_calendar_id = excluded.google_calendar_id,
            google_event_id = excluded.google_event_id,
            updated_at = excluded.updated_at",
        params![
            e.id,
            e.title,
            e.description,
            e.date,
            e.hour as i64,
            e.minute as i64,
            e.all_day as i64,
            e.end_hour.map(|v| v as i64),
            e.end_minute.map(|v| v as i64),
            e.repeat.map(repeat_to_str),
            e.message,
            e.enabled as i64,
            e.last_fired,
            e.source,
            e.google_calendar_id,
            e.google_event_id,
            e.updated_at,
        ],
    )
    .map_err(|e| format!("store event: {e}"))?;
    Ok(())
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarEvent> {
    let repeat: Option<String> = r.get(9)?;
    Ok(CalendarEvent {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        date: r.get(3)?,
        hour: r.get::<_, i64>(4).unwrap_or(9) as u8,
        minute: r.get::<_, i64>(5).unwrap_or(0) as u8,
        all_day: r.get::<_, i64>(6).unwrap_or(0) != 0,
        end_hour: r.get::<_, Option<i64>>(7)?.map(|v| v as u8),
        end_minute: r.get::<_, Option<i64>>(8)?.map(|v| v as u8),
        repeat: repeat.as_deref().and_then(repeat_from_str),
        // Legacy flag is normalized away by the JSON migration; rows are
        // always written with `repeat` populated by sanitize/op_save.
        repeat_yearly: false,
        message: r.get(10)?,
        enabled: r.get::<_, i64>(11).unwrap_or(1) != 0,
        last_fired: r.get(12)?,
        source: r.get(13)?,
        google_calendar_id: r.get(14)?,
        google_event_id: r.get(15)?,
        updated_at: r.get(16)?,
    })
}

fn repeat_to_str(rule: Repeat) -> &'static str {
    match rule {
        Repeat::None => "none",
        Repeat::Daily => "daily",
        Repeat::Weekdays => "weekdays",
        Repeat::Weekly => "weekly",
        Repeat::Monthly => "monthly",
        Repeat::Yearly => "yearly",
    }
}

fn repeat_from_str(s: &str) -> Option<Repeat> {
    match s {
        "none" => Some(Repeat::None),
        "daily" => Some(Repeat::Daily),
        "weekdays" => Some(Repeat::Weekdays),
        "weekly" => Some(Repeat::Weekly),
        "monthly" => Some(Repeat::Monthly),
        "yearly" => Some(Repeat::Yearly),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "calendar-store-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn event(id: &str, date: &str) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            title: "Dentist".into(),
            description: "checkup".into(),
            date: date.into(),
            hour: 15,
            minute: 30,
            all_day: false,
            end_hour: Some(16),
            end_minute: Some(0),
            repeat: Some(Repeat::Weekly),
            repeat_yearly: false,
            message: "hello".into(),
            enabled: true,
            last_fired: Some(12345),
            source: None,
            google_calendar_id: None,
            google_event_id: None,
            updated_at: None,
        }
    }

    #[test]
    fn event_roundtrip() {
        let store = Store::open(&temp_dir("roundtrip")).unwrap();
        let mut e = event("e1", "2030-03-10");
        store.upsert_event(&e).unwrap();
        e.title = "Edited".into();
        e.enabled = false;
        store.upsert_event(&e).unwrap(); // upsert replaces
        let list = store.load_events();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Edited");
        assert!(!list[0].enabled);
        assert_eq!(list[0].repeat, Some(Repeat::Weekly));
        assert_eq!(list[0].end_hour, Some(16));
        assert_eq!(list[0].last_fired, Some(12345));

        store.delete_event("e1").unwrap();
        assert_eq!(store.count_events(), 0);
    }

    #[test]
    fn migration_imports_legacy_json_and_renames() {
        let dir = temp_dir("migrate");
        let legacy = serde_json::json!([
            { "id": "old1", "title": "Birthday", "date": "1990-04-01", "hour": 9, "minute": 0, "repeat_yearly": true },
            { "id": "old2", "date": "2030-01-02", "hour": 8, "minute": 15 }
        ]);
        std::fs::write(dir.join(LEGACY_JSON), legacy.to_string()).unwrap();
        let store = Store::open(&dir).unwrap();
        let n = store.migrate_from_json(&dir).unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.count_events(), 2);
        let list = store.load_events();
        assert_eq!(list[0].id, "old1");
        // Legacy yearly bool normalized into the repeat rule.
        assert_eq!(list[0].repeat, Some(Repeat::Yearly));
        assert_eq!(list[0].repeat_rule(), Repeat::Yearly);
        assert_eq!(list[1].hour, 8);
        assert!(dir.join(LEGACY_JSON_BAK).exists());
        assert!(!dir.join(LEGACY_JSON).exists());
        // Second call is a no-op (DB already populated).
        assert_eq!(store.migrate_from_json(&dir).unwrap(), 0);
    }

    #[test]
    fn google_row_helpers() {
        let store = Arc::new(Store::open(&temp_dir("google")).unwrap());
        let mut e = event("gcal-cal-abc", "2030-03-10");
        e.source = Some("google".into());
        e.google_calendar_id = Some("cal".into());
        e.google_event_id = Some("abc".into());
        store.upsert_event(&e).unwrap();

        store.delete_google_rows("cal", "abc").unwrap();
        assert_eq!(store.count_events(), 0);

        store.upsert_event(&e).unwrap();
        store.set_google_ids("gcal-cal-abc", "cal2", "xyz", 999).unwrap();
        let list = store.load_events();
        assert_eq!(list[0].google_calendar_id.as_deref(), Some("cal2"));
        assert_eq!(list[0].google_event_id.as_deref(), Some("xyz"));
        assert_eq!(list[0].updated_at, Some(999));
    }

    #[test]
    fn unified_migration_drops_instances_and_clears_tokens() {
        let store = Arc::new(Store::open(&temp_dir("unified")).unwrap());
        let mut master = event("gcal-cal-abc", "2030-03-10");
        master.source = Some("google".into());
        master.google_calendar_id = Some("cal".into());
        master.google_event_id = Some("abc".into());
        let mut instance = event("gcal-cal-abc-2030-03-10", "2030-03-10");
        instance.source = Some("google".into());
        instance.google_calendar_id = Some("cal".into());
        instance.google_event_id = Some("abc".into());
        let mut local = event("event-123", "2030-03-11");
        local.source = None;
        store.upsert_events(&[master, instance, local]).unwrap();
        store.sync_state_set("cal", Some("tok")).unwrap();

        // First run migrates; a second run is a no-op.
        store.migrate_unified_rows().unwrap();
        let list = store.load_events();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|e| e.id != "gcal-cal-abc-2030-03-10"));
        assert_eq!(store.sync_token_get("cal"), None);
        store.migrate_unified_rows().unwrap();
        assert_eq!(store.load_events().len(), 2);
    }

    #[test]
    fn config_and_state() {
        let store = Store::open(&temp_dir("config")).unwrap();
        assert_eq!(store.config_get("client_id"), None);
        store.config_set("client_id", "abc.apps.googleusercontent.com").unwrap();
        store.config_set("client_id", "def").unwrap();
        assert_eq!(store.config_get("client_id").as_deref(), Some("def"));
        store.config_delete("client_id").unwrap();
        assert_eq!(store.config_get("client_id"), None);

        assert_eq!(store.sync_token_get("cal-a"), None);
        store.sync_state_set("cal-a", Some("tok1")).unwrap();
        assert_eq!(store.sync_token_get("cal-a").as_deref(), Some("tok1"));
        store.sync_state_set("cal-a", None).unwrap();
        assert_eq!(store.sync_token_get("cal-a"), None);
        assert!(store.last_sync().is_some());
        store.sync_state_clear().unwrap();
        assert_eq!(store.last_sync(), None);
    }

    #[test]
    fn queue_fifo_and_helpers() {
        let store = Store::open(&temp_dir("queue")).unwrap();
        assert_eq!(store.queue_count(), 0);
        store.queue_add("a", "upsert", None).unwrap();
        store
            .queue_add("b", "delete", Some(r#"{"calendar_id":"c","event_id":"g"}"#))
            .unwrap();
        assert_eq!(store.queue_count(), 2);

        let first = store.queue_next().unwrap();
        assert_eq!(first.local_id, "a");
        assert_eq!(first.op, "upsert");
        store.queue_touch(first.seq).unwrap();
        store.queue_remove(first.seq).unwrap();

        let next = store.queue_next().unwrap();
        assert_eq!(next.local_id, "b");
        assert_eq!(next.attempts, 0);

        let pending = store.queue_pending_upsert_ids();
        assert!(pending.contains("b") == false); // b is a delete
        store.queue_add("c", "upsert", None).unwrap();
        assert!(store.queue_pending_upsert_ids().contains("c"));

        store.queue_remove_for_local("b").unwrap();
        assert_eq!(store.queue_count(), 1);
        store.queue_clear().unwrap();
        assert_eq!(store.queue_count(), 0);
    }
}
