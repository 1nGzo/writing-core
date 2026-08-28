use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{Duration as ChronoDuration, Local, LocalResult, NaiveDate, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::writing::{count_writing_units, WritingCount, WRITING_COUNT_VERSION};

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("no Markdown document is open")]
    NoDocument,
    #[error("event belongs to another document")]
    WrongDocument,
    #[error("unsafe save refused because the file changed externally")]
    ExternalChange,
    #[error("document path does not match the open document")]
    WrongPath,
    #[error(
        "invalid runtime timing: idle_threshold_seconds must be > 0 and session_end_threshold_seconds must be greater than idle_threshold_seconds"
    )]
    InvalidRuntimeTiming,
    #[error("daily stats days must be between 1 and 31")]
    InvalidDailyStatsDays,
}

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub idle_seconds: i64,
    pub session_end_seconds: i64,
    source: &'static str,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct RuntimeTimingConfig {
    pub idle_threshold_seconds: i64,
    pub session_end_threshold_seconds: i64,
    pub mode: String,
    pub source: String,
    pub adjustable: bool,
}

impl CoreConfig {
    pub fn from_env() -> Self {
        let requested_idle = env_seconds("WRITING_SPIKE_IDLE_SECONDS", 300);
        let requested_session_end = env_seconds("WRITING_SPIKE_SESSION_END_SECONDS", 3600);
        let source = match (
            env::var_os("WRITING_SPIKE_IDLE_SECONDS").is_some(),
            env::var_os("WRITING_SPIKE_SESSION_END_SECONDS").is_some(),
        ) {
            (false, false) => "default",
            (true, true) => "environment",
            _ => "environment/default",
        };
        match Self::with_source(requested_idle, requested_session_end, source) {
            Ok(config) => config,
            Err(_) => {
                let mut defaults =
                    Self::new(300, 3600).expect("built-in runtime timing defaults are valid");
                defaults.source = "default (invalid environment)";
                defaults
            }
        }
    }

    pub fn new(idle_seconds: i64, session_end_seconds: i64) -> Result<Self, CoreError> {
        Self::with_source(idle_seconds, session_end_seconds, "constructor")
    }

    fn with_source(
        idle_seconds: i64,
        session_end_seconds: i64,
        source: &'static str,
    ) -> Result<Self, CoreError> {
        if idle_seconds <= 0 || session_end_seconds <= idle_seconds {
            return Err(CoreError::InvalidRuntimeTiming);
        }
        Ok(Self {
            idle_seconds,
            session_end_seconds,
            source,
        })
    }

    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub fn apply_debug_timing(
        &mut self,
        idle_seconds: i64,
        session_end_seconds: i64,
    ) -> Result<(), CoreError> {
        let validated = Self::with_source(idle_seconds, session_end_seconds, "debug override")?;
        self.idle_seconds = validated.idle_seconds;
        self.session_end_seconds = validated.session_end_seconds;
        self.source = validated.source;
        Ok(())
    }

    pub fn runtime_timing(&self) -> RuntimeTimingConfig {
        RuntimeTimingConfig {
            idle_threshold_seconds: self.idle_seconds,
            session_end_threshold_seconds: self.session_end_seconds,
            mode: if cfg!(debug_assertions) {
                "DEVELOPMENT".to_string()
            } else {
                "PRODUCTION".to_string()
            },
            source: self.source.to_string(),
            adjustable: cfg!(debug_assertions),
        }
    }

    fn active_grace_seconds(&self) -> i64 {
        self.idle_seconds.min(self.session_end_seconds)
    }
}

fn env_seconds(name: &str, default: i64) -> i64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DocumentSnapshot {
    pub document_id: String,
    pub path: String,
    pub content: String,
    pub hash: String,
    pub mtime_ms: i64,
    pub size: u64,
    pub recovery_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct EditEvent {
    pub document_id: String,
    pub timestamp_ms: i64,
    pub source: String,
    pub tracking_origin: String,
    pub inserted_chars: i64,
    pub deleted_chars: i64,
    #[serde(default)]
    pub writing_inserted_units: Option<i64>,
    #[serde(default)]
    pub writing_deleted_units: Option<i64>,
    #[serde(default)]
    pub writing_count_version: Option<i64>,
    pub is_user_edit: bool,
    pub ime_composition: bool,
    #[serde(rename = "event_id", alias = "transaction_id")]
    pub transaction_id: String,
    pub change_count: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CoreStatus {
    pub document_id: Option<String>,
    pub session_id: Option<String>,
    pub session_started_at: Option<i64>,
    pub state: String,
    pub since_last_edit_seconds: i64,
    pub session_span_seconds: i64,
    pub session_active_seconds: i64,
    pub session_idle_seconds: i64,
    pub session_inserted_chars: i64,
    pub session_deleted_chars: i64,
    pub session_net_chars: i64,
    pub session_writing_inserted_units: Option<i64>,
    pub session_writing_deleted_units: Option<i64>,
    pub session_writing_net_units: Option<i64>,
    pub session_writing_count_version: Option<i64>,
    pub document_writing_count: Option<i64>,
    pub tracking_origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct DailyWritingStat {
    pub date: String,
    pub writing_net_units: i64,
    pub active_seconds: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct DailyStatsResponse {
    pub schema_version: u8,
    pub days: Vec<DailyWritingStat>,
}

#[derive(Debug, Clone)]
struct CurrentDocument {
    id: String,
    path: PathBuf,
    hash: String,
    writing_count: Option<WritingCount>,
}

#[derive(Debug, Clone)]
struct SessionRow {
    id: String,
    started_at: i64,
    last_edit_at: i64,
    ended_at: Option<i64>,
    status: String,
    tracking_origin: String,
    inserted_chars: i64,
    deleted_chars: i64,
    net_chars: i64,
    active_seconds: i64,
    idle_seconds: i64,
    writing_inserted_units: Option<i64>,
    writing_deleted_units: Option<i64>,
    writing_net_units: Option<i64>,
    writing_count_version: Option<i64>,
}

pub struct WritingCore {
    storage: Storage,
    config: CoreConfig,
    current_document: Option<CurrentDocument>,
}

impl WritingCore {
    pub fn new(db_path: PathBuf, config: CoreConfig) -> Result<Self, CoreError> {
        let storage = Storage::open(&db_path)?;
        Ok(Self {
            storage,
            config,
            current_document: None,
        })
    }

    pub fn open_document(&mut self, snapshot: &DocumentSnapshot) -> Result<String, CoreError> {
        let writing_count = Some(count_writing_units(&snapshot.content));
        let id = self.storage.ensure_document(snapshot, writing_count)?;
        self.current_document = Some(CurrentDocument {
            id: id.clone(),
            path: PathBuf::from(&snapshot.path),
            hash: snapshot.hash.clone(),
            writing_count,
        });
        Ok(id)
    }

    pub fn close_document(&mut self) {
        self.current_document = None;
    }

    pub fn runtime_timing(&self) -> RuntimeTimingConfig {
        self.config.runtime_timing()
    }

    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub fn apply_debug_timing(
        &mut self,
        idle_seconds: i64,
        session_end_seconds: i64,
    ) -> Result<RuntimeTimingConfig, CoreError> {
        self.config
            .apply_debug_timing(idle_seconds, session_end_seconds)?;
        Ok(self.runtime_timing())
    }

    pub fn ensure_save_is_safe(
        &self,
        document_id: &str,
        path: &Path,
        current_hash: &str,
    ) -> Result<(), CoreError> {
        let Some(document) = &self.current_document else {
            return Err(CoreError::NoDocument);
        };
        if document.id != document_id {
            return Err(CoreError::WrongDocument);
        }
        if document.path != path {
            return Err(CoreError::WrongPath);
        }
        if document.hash != current_hash {
            return Err(CoreError::ExternalChange);
        }
        Ok(())
    }

    pub fn mark_saved(
        &mut self,
        hash: &str,
        mtime_ms: i64,
        size: u64,
        content: &str,
    ) -> Result<(), CoreError> {
        let Some(document) = self.current_document.as_mut() else {
            return Err(CoreError::NoDocument);
        };
        let writing_count = Some(count_writing_units(content));
        document.hash = hash.to_string();
        document.writing_count = writing_count;
        self.storage
            .update_document_state(&document.id, hash, mtime_ms, size, writing_count)
    }

    pub fn mark_observed_snapshot(
        &mut self,
        hash: &str,
        mtime_ms: i64,
        size: u64,
        content: &str,
    ) -> Result<(), CoreError> {
        let Some(document) = self.current_document.as_mut() else {
            return Err(CoreError::NoDocument);
        };
        let writing_count = Some(count_writing_units(content));
        document.hash = hash.to_string();
        document.writing_count = writing_count;
        self.storage
            .update_document_state(&document.id, hash, mtime_ms, size, writing_count)
    }

    pub fn mark_live_snapshot(&mut self, content: &str) -> Result<(), CoreError> {
        let Some(document) = self.current_document.as_mut() else {
            return Err(CoreError::NoDocument);
        };
        let writing_count = Some(count_writing_units(content));
        document.writing_count = writing_count;
        self.storage
            .update_document_writing_count(&document.id, writing_count)
    }

    pub fn record_edit(&mut self, event: EditEvent) -> Result<CoreStatus, CoreError> {
        self.record_edit_at(event, epoch_seconds())
    }

    fn record_edit_at(
        &mut self,
        event: EditEvent,
        status_now: i64,
    ) -> Result<CoreStatus, CoreError> {
        let Some(document) = &self.current_document else {
            return Err(CoreError::NoDocument);
        };
        if document.id != event.document_id {
            return Err(CoreError::WrongDocument);
        }

        self.storage
            .record_event(&document.id, event, &self.config)?;
        self.status_at(status_now)
    }

    pub fn status(&mut self) -> Result<CoreStatus, CoreError> {
        self.status_at_with_persistence(epoch_seconds(), true)
    }

    fn status_at(&mut self, now: i64) -> Result<CoreStatus, CoreError> {
        self.status_at_with_persistence(now, true)
    }

    pub fn status_read_only(&mut self) -> Result<CoreStatus, CoreError> {
        self.status_at_with_persistence(epoch_seconds(), false)
    }

    // Used when tracking stops: the open session must close under the same
    // rules as an automatic end so a later edit never resumes it.
    pub fn end_current_session(&mut self) -> Result<bool, CoreError> {
        self.end_current_session_at(epoch_seconds())
    }

    fn end_current_session_at(&mut self, now: i64) -> Result<bool, CoreError> {
        let Some(document) = &self.current_document else {
            return Ok(false);
        };
        let Some(session) = self.storage.latest_session(&document.id)? else {
            return Ok(false);
        };
        if session.ended_at.is_some() {
            return Ok(false);
        }

        // Follow the automatic-end boundary used by status/record_event: a
        // session never extends past its last edit by more than the
        // session-end threshold, even when the close arrives later.
        let requested_end = now.max(session.last_edit_at);
        let gap = requested_end.saturating_sub(session.last_edit_at);
        let effective_end = session.last_edit_at + gap.min(self.config.session_end_seconds);
        let active_increment = gap.min(self.config.active_grace_seconds());
        let idle_increment = effective_end
            .saturating_sub(session.last_edit_at)
            .saturating_sub(active_increment);
        self.storage.end_session(
            &session.id,
            effective_end,
            session.active_seconds + active_increment,
            session.idle_seconds + idle_increment,
        )?;
        Ok(true)
    }

    pub fn daily_stats(&self, days: usize) -> Result<DailyStatsResponse, CoreError> {
        if !(1..=31).contains(&days) {
            return Err(CoreError::InvalidDailyStatsDays);
        }
        self.storage
            .daily_stats_at(epoch_seconds(), days, &self.config)
    }

    fn daily_stats_at(&self, now: i64, days: usize) -> Result<DailyStatsResponse, CoreError> {
        if !(1..=31).contains(&days) {
            return Err(CoreError::InvalidDailyStatsDays);
        }
        self.storage.daily_stats_at(now, days, &self.config)
    }

    fn status_at_with_persistence(
        &mut self,
        now: i64,
        persist_session_end: bool,
    ) -> Result<CoreStatus, CoreError> {
        let Some(document) = &self.current_document else {
            let status = waiting_status();
            debug_core_status(&status, &self.config);
            return Ok(status);
        };

        let Some(mut session) = self.storage.latest_session(&document.id)? else {
            let status = waiting_status_for_document(&document.id, document.writing_count);
            debug_core_status(&status, &self.config);
            return Ok(status);
        };

        let gap = (now - session.last_edit_at).max(0);
        let since_last_edit_seconds = gap;
        if session.ended_at.is_none() && gap >= self.config.session_end_seconds {
            let active_increment = gap.min(self.config.active_grace_seconds());
            let idle_increment = self
                .config
                .session_end_seconds
                .saturating_sub(active_increment);
            let ended_at = session.last_edit_at + self.config.session_end_seconds;
            if persist_session_end {
                self.storage.end_session(
                    &session.id,
                    ended_at,
                    session.active_seconds + active_increment,
                    session.idle_seconds + idle_increment,
                )?;
            }
            session.status = "ended".to_string();
            session.ended_at = Some(ended_at);
            session.active_seconds += active_increment;
            session.idle_seconds += idle_increment;
        }

        let state = if session.ended_at.is_some() {
            "ended".to_string()
        } else if gap >= self.config.idle_seconds {
            "idle".to_string()
        } else {
            "active".to_string()
        };
        let end_time = session.ended_at.unwrap_or(now);
        let span = (end_time - session.started_at).max(0);
        let active_seconds = if session.ended_at.is_none() {
            session.active_seconds + gap.min(self.config.active_grace_seconds())
        } else {
            session.active_seconds
        };
        let idle_seconds = if session.ended_at.is_none() {
            session.idle_seconds
                + gap
                    .saturating_sub(self.config.active_grace_seconds())
                    .max(0)
        } else {
            session.idle_seconds
        };

        let status = CoreStatus {
            document_id: Some(document.id.clone()),
            session_id: Some(session.id),
            session_started_at: Some(session.started_at),
            state,
            since_last_edit_seconds,
            session_span_seconds: span,
            session_active_seconds: active_seconds,
            session_idle_seconds: idle_seconds,
            session_inserted_chars: session.inserted_chars,
            session_deleted_chars: session.deleted_chars,
            session_net_chars: session.net_chars,
            session_writing_inserted_units: session.writing_inserted_units,
            session_writing_deleted_units: session.writing_deleted_units,
            session_writing_net_units: session.writing_net_units,
            session_writing_count_version: session.writing_count_version,
            document_writing_count: document.writing_count.map(|count| count.total),
            tracking_origin: Some(session.tracking_origin),
        };
        debug_core_status(&status, &self.config);
        Ok(status)
    }
}

fn waiting_status() -> CoreStatus {
    waiting_status_for_document_with_session(None, None)
}

fn waiting_status_for_document(
    document_id: &str,
    writing_count: Option<WritingCount>,
) -> CoreStatus {
    waiting_status_for_document_with_session(Some(document_id.to_string()), writing_count)
}

fn waiting_status_for_document_with_session(
    document_id: Option<String>,
    writing_count: Option<WritingCount>,
) -> CoreStatus {
    CoreStatus {
        document_id,
        session_id: None,
        session_started_at: None,
        state: "waiting".to_string(),
        since_last_edit_seconds: 0,
        session_span_seconds: 0,
        session_active_seconds: 0,
        session_idle_seconds: 0,
        session_inserted_chars: 0,
        session_deleted_chars: 0,
        session_net_chars: 0,
        session_writing_inserted_units: None,
        session_writing_deleted_units: None,
        session_writing_net_units: None,
        session_writing_count_version: None,
        document_writing_count: writing_count.map(|count| count.total),
        tracking_origin: None,
    }
}

#[cfg(debug_assertions)]
fn debug_core_status(status: &CoreStatus, config: &CoreConfig) {
    let invariant_delta =
        (status.session_span_seconds - status.session_active_seconds - status.session_idle_seconds)
            .abs();
    eprintln!(
        "[CORE STATUS] span={} active={} idle={} since_last_edit={} state={} idle_threshold={} session_end={} document_id={} session_id={}",
        status.session_span_seconds,
        status.session_active_seconds,
        status.session_idle_seconds,
        status.since_last_edit_seconds,
        status.state,
        config.idle_seconds,
        config.session_end_seconds,
        status.document_id.as_deref().unwrap_or("-"),
        status.session_id.as_deref().unwrap_or("-"),
    );
    if invariant_delta > 1 {
        eprintln!(
            "[WRITING CORE WARNING] session time invariant violated span={} active={} idle={} delta={}",
            status.session_span_seconds,
            status.session_active_seconds,
            status.session_idle_seconds,
            invariant_delta,
        );
    }
}

#[cfg(not(debug_assertions))]
fn debug_core_status(_status: &CoreStatus, _config: &CoreConfig) {}

fn epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

struct Storage {
    conn: Connection,
}

impl Storage {
    fn open(path: &Path) -> Result<Self, CoreError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|_| rusqlite::Error::InvalidPath(parent.to_path_buf()))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let storage = Self { conn };
        storage.migrate()?;
        Ok(storage)
    }

    fn migrate(&self) -> Result<(), CoreError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (id INTEGER PRIMARY KEY CHECK (id = 1), version INTEGER NOT NULL);
             INSERT OR IGNORE INTO schema_meta (id, version) VALUES (1, 0);",
        )?;
        let version: i64 =
            self.conn
                .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                    row.get(0)
                })?;
        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE documents (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    last_hash TEXT NOT NULL,
                    last_mtime_ms INTEGER NOT NULL,
                    last_size INTEGER NOT NULL
                );
                CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    document_id TEXT NOT NULL,
                    started_at INTEGER NOT NULL,
                    last_edit_at INTEGER NOT NULL,
                    ended_at INTEGER,
                    status TEXT NOT NULL,
                    inserted_chars INTEGER NOT NULL DEFAULT 0,
                    deleted_chars INTEGER NOT NULL DEFAULT 0,
                    net_chars INTEGER NOT NULL DEFAULT 0,
                    active_seconds INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX sessions_document_idx ON sessions(document_id, started_at);
                CREATE TABLE edit_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT,
                    document_id TEXT NOT NULL,
                    timestamp_ms INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    inserted_chars INTEGER NOT NULL,
                    deleted_chars INTEGER NOT NULL,
                    net_chars INTEGER NOT NULL,
                    is_user_edit INTEGER NOT NULL,
                    ime_composition INTEGER NOT NULL,
                    transaction_id TEXT NOT NULL,
                    change_count INTEGER NOT NULL
                );
                CREATE INDEX edit_events_document_idx ON edit_events(document_id, timestamp_ms);
                UPDATE schema_meta SET version = 1 WHERE id = 1;",
            )?;
        }
        let version: i64 =
            self.conn
                .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                    row.get(0)
                })?;
        if version < 2 {
            self.conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN idle_seconds INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN tracking_origin TEXT NOT NULL DEFAULT 'editor_transaction';
                 ALTER TABLE edit_events ADD COLUMN tracking_origin TEXT NOT NULL DEFAULT 'editor_transaction';
                 UPDATE schema_meta SET version = 2 WHERE id = 1;",
            )?;
        }
        let version: i64 =
            self.conn
                .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                    row.get(0)
                })?;
        if version < 3 {
            self.conn.execute_batch(
                "ALTER TABLE documents ADD COLUMN current_writing_units INTEGER;
                 ALTER TABLE documents ADD COLUMN writing_count_version INTEGER;
                 ALTER TABLE sessions ADD COLUMN writing_inserted_units INTEGER;
                 ALTER TABLE sessions ADD COLUMN writing_deleted_units INTEGER;
                 ALTER TABLE sessions ADD COLUMN writing_net_units INTEGER;
                 ALTER TABLE sessions ADD COLUMN writing_count_version INTEGER;
                 ALTER TABLE edit_events ADD COLUMN writing_inserted_units INTEGER;
                 ALTER TABLE edit_events ADD COLUMN writing_deleted_units INTEGER;
                 ALTER TABLE edit_events ADD COLUMN writing_net_units INTEGER;
                 ALTER TABLE edit_events ADD COLUMN writing_count_version INTEGER;
                 UPDATE schema_meta SET version = 3 WHERE id = 1;",
            )?;
        }
        Ok(())
    }

    fn ensure_document(
        &self,
        snapshot: &DocumentSnapshot,
        writing_count: Option<WritingCount>,
    ) -> Result<String, CoreError> {
        let now = epoch_seconds();
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM documents WHERE path = ?1",
                params![snapshot.path],
                |row| row.get(0),
            )
            .optional()?;
        let id = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
        self.conn.execute(
            "INSERT INTO documents (id, path, created_at, updated_at, last_hash, last_mtime_ms, last_size,
                 current_writing_units, writing_count_version)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET updated_at = excluded.updated_at,
                 last_hash = excluded.last_hash, last_mtime_ms = excluded.last_mtime_ms,
                 last_size = excluded.last_size,
                 current_writing_units = excluded.current_writing_units,
                 writing_count_version = excluded.writing_count_version",
            params![
                id,
                snapshot.path,
                now,
                snapshot.hash,
                snapshot.mtime_ms,
                snapshot.size as i64,
                writing_count.map(|count| count.total),
                writing_count.map(|_| WRITING_COUNT_VERSION),
            ],
        )?;
        Ok(id)
    }

    fn update_document_state(
        &self,
        document_id: &str,
        hash: &str,
        mtime_ms: i64,
        size: u64,
        writing_count: Option<WritingCount>,
    ) -> Result<(), CoreError> {
        self.conn.execute(
            "UPDATE documents SET updated_at = ?2, last_hash = ?3, last_mtime_ms = ?4, last_size = ?5,
                 current_writing_units = ?6, writing_count_version = ?7 WHERE id = ?1",
            params![
                document_id,
                epoch_seconds(),
                hash,
                mtime_ms,
                size as i64,
                writing_count.map(|count| count.total),
                writing_count.map(|_| WRITING_COUNT_VERSION),
            ],
        )?;
        Ok(())
    }

    fn update_document_writing_count(
        &self,
        document_id: &str,
        writing_count: Option<WritingCount>,
    ) -> Result<(), CoreError> {
        self.conn.execute(
            "UPDATE documents SET updated_at = ?2, current_writing_units = ?3,
                 writing_count_version = ?4 WHERE id = ?1",
            params![
                document_id,
                epoch_seconds(),
                writing_count.map(|count| count.total),
                writing_count.map(|_| WRITING_COUNT_VERSION),
            ],
        )?;
        Ok(())
    }

    fn latest_session(&self, document_id: &str) -> Result<Option<SessionRow>, CoreError> {
        self.conn
            .query_row(
                "SELECT id, started_at, last_edit_at, ended_at, status, tracking_origin,
                        inserted_chars, deleted_chars, net_chars, active_seconds, idle_seconds,
                        writing_inserted_units, writing_deleted_units, writing_net_units,
                        writing_count_version
                 FROM sessions WHERE document_id = ?1 ORDER BY started_at DESC LIMIT 1",
                params![document_id],
                |row| {
                    Ok(SessionRow {
                        id: row.get(0)?,
                        started_at: row.get(1)?,
                        last_edit_at: row.get(2)?,
                        ended_at: row.get(3)?,
                        status: row.get(4)?,
                        tracking_origin: row.get(5)?,
                        inserted_chars: row.get(6)?,
                        deleted_chars: row.get(7)?,
                        net_chars: row.get(8)?,
                        active_seconds: row.get(9)?,
                        idle_seconds: row.get(10)?,
                        writing_inserted_units: row.get(11)?,
                        writing_deleted_units: row.get(12)?,
                        writing_net_units: row.get(13)?,
                        writing_count_version: row.get(14)?,
                    })
                },
            )
            .optional()
            .map_err(CoreError::from)
    }

    fn end_session(
        &self,
        session_id: &str,
        ended_at: i64,
        active_seconds: i64,
        idle_seconds: i64,
    ) -> Result<(), CoreError> {
        self.conn.execute(
            "UPDATE sessions SET status = 'ended', ended_at = ?2, active_seconds = ?3, idle_seconds = ?4
             WHERE id = ?1 AND ended_at IS NULL",
            params![session_id, ended_at, active_seconds, idle_seconds],
        )?;
        Ok(())
    }

    fn daily_stats_at(
        &self,
        now: i64,
        days: usize,
        config: &CoreConfig,
    ) -> Result<DailyStatsResponse, CoreError> {
        let today = local_date(now);
        let dates: Vec<NaiveDate> = (0..days)
            .rev()
            .map(|offset| today - ChronoDuration::days(offset as i64))
            .collect();
        let mut totals: HashMap<NaiveDate, (i64, i64)> =
            dates.iter().copied().map(|date| (date, (0, 0))).collect();

        let sessions: Vec<(String, i64, i64, Option<i64>)> = {
            let mut statement = self.conn.prepare(
                "SELECT id, started_at, last_edit_at, ended_at
                 FROM sessions ORDER BY started_at ASC, id ASC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        for (session_id, started_at, last_edit_at, ended_at) in sessions {
            if started_at > now {
                continue;
            }
            let events: Vec<(i64, Option<i64>, Option<i64>)> = {
                let mut statement = self.conn.prepare(
                    "SELECT timestamp_ms, writing_net_units, writing_count_version
                     FROM edit_events
                     WHERE session_id = ?1
                     ORDER BY timestamp_ms ASC, id ASC",
                )?;
                let rows = statement
                    .query_map(params![session_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };

            let effective_end = match ended_at {
                Some(ended_at) => ended_at.min(now),
                None => now.min(last_edit_at.saturating_add(config.session_end_seconds)),
            }
            .max(started_at);
            for (timestamp_ms, writing_net_units, writing_count_version) in &events {
                let timestamp = timestamp_ms.div_euclid(1_000);
                if timestamp < started_at || timestamp > effective_end {
                    continue;
                }
                if *writing_count_version == Some(WRITING_COUNT_VERSION) {
                    if let Some(net_units) = writing_net_units {
                        add_daily_net(&mut totals, local_date(timestamp), *net_units);
                    }
                }
            }

            let event_seconds: Vec<i64> = events
                .iter()
                .map(|(timestamp_ms, _, _)| timestamp_ms.div_euclid(1_000))
                .filter(|timestamp| *timestamp >= started_at && *timestamp <= effective_end)
                .collect();
            for (index, event_start) in event_seconds.iter().copied().enumerate() {
                let next_event = event_seconds.get(index + 1).copied();
                let interval_end = next_event
                    .unwrap_or(effective_end)
                    .min(effective_end)
                    .min(event_start + config.active_grace_seconds());
                if interval_end > event_start {
                    add_active_interval(&mut totals, event_start, interval_end);
                }
            }
        }

        Ok(DailyStatsResponse {
            schema_version: 1,
            days: dates
                .into_iter()
                .map(|date| {
                    let (writing_net_units, active_seconds) =
                        totals.get(&date).copied().unwrap_or_default();
                    DailyWritingStat {
                        date: date.format("%Y-%m-%d").to_string(),
                        writing_net_units,
                        active_seconds,
                    }
                })
                .collect(),
        })
    }

    fn record_event(
        &self,
        document_id: &str,
        event: EditEvent,
        config: &CoreConfig,
    ) -> Result<(), CoreError> {
        if !event.is_user_edit {
            return Ok(());
        }

        let already_recorded: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM edit_events WHERE transaction_id = ?1 LIMIT 1",
                params![event.transaction_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if already_recorded.is_some() {
            return Ok(());
        }

        let timestamp = event.timestamp_ms / 1000;
        let mut session = self
            .latest_session(document_id)?
            .filter(|session| session.ended_at.is_none());

        if let Some(previous) = &session {
            let gap = (timestamp - previous.last_edit_at).max(0);
            if gap >= config.session_end_seconds {
                let active_increment = gap.min(config.active_grace_seconds());
                let idle_increment = config.session_end_seconds.saturating_sub(active_increment);
                self.end_session(
                    &previous.id,
                    previous.last_edit_at + config.session_end_seconds,
                    previous.active_seconds + active_increment,
                    previous.idle_seconds + idle_increment,
                )?;
                session = None;
            }
        }

        let session_id = if let Some(current) = &session {
            current.id.clone()
        } else {
            let id = Uuid::new_v4().to_string();
            self.conn.execute(
                "INSERT INTO sessions (id, document_id, started_at, last_edit_at, status, tracking_origin,
                    writing_inserted_units, writing_deleted_units, writing_net_units, writing_count_version)
                 VALUES (?1, ?2, ?3, ?3, 'active', ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    document_id,
                    timestamp,
                    event.tracking_origin,
                    event.writing_inserted_units,
                    event.writing_deleted_units,
                    writing_net(&event),
                    event.writing_count_version,
                ],
            )?;
            id
        };

        let (active_increment, idle_increment, session_origin) = if let Some(current) = &session {
            let gap = (timestamp - current.last_edit_at).max(0);
            let active_increment = gap.min(config.active_grace_seconds());
            let idle_increment = gap.saturating_sub(active_increment);
            let session_origin = if current.tracking_origin == event.tracking_origin {
                current.tracking_origin.clone()
            } else {
                "mixed".to_string()
            };
            (active_increment, idle_increment, session_origin)
        } else {
            (0, 0, event.tracking_origin.clone())
        };

        let net = event.inserted_chars - event.deleted_chars;
        let (writing_inserted, writing_deleted, writing_net_units, writing_version) =
            if let Some(current) = &session {
                (
                    add_optional(current.writing_inserted_units, event.writing_inserted_units),
                    add_optional(current.writing_deleted_units, event.writing_deleted_units),
                    add_optional(current.writing_net_units, writing_net(&event)),
                    matching_writing_version(
                        current.writing_count_version,
                        event.writing_count_version,
                    ),
                )
            } else {
                (
                    event.writing_inserted_units,
                    event.writing_deleted_units,
                    writing_net(&event),
                    event.writing_count_version,
                )
            };
        self.conn.execute(
            "UPDATE sessions SET last_edit_at = ?2, status = 'active', tracking_origin = ?3,
                inserted_chars = inserted_chars + ?4,
                deleted_chars = deleted_chars + ?5,
                net_chars = net_chars + ?6,
                active_seconds = active_seconds + ?7,
                idle_seconds = idle_seconds + ?8,
                writing_inserted_units = ?9,
                writing_deleted_units = ?10,
                writing_net_units = ?11,
                writing_count_version = ?12
             WHERE id = ?1",
            params![
                session_id,
                timestamp,
                session_origin,
                event.inserted_chars,
                event.deleted_chars,
                net,
                active_increment,
                idle_increment,
                writing_inserted,
                writing_deleted,
                writing_net_units,
                writing_version,
            ],
        )?;

        self.conn.execute(
            "INSERT INTO edit_events (session_id, document_id, timestamp_ms, source,
                tracking_origin, inserted_chars, deleted_chars, net_chars, is_user_edit,
                ime_composition, transaction_id, change_count, writing_inserted_units,
                writing_deleted_units, writing_net_units, writing_count_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                session_id,
                document_id,
                event.timestamp_ms,
                event.source,
                event.tracking_origin,
                event.inserted_chars,
                event.deleted_chars,
                net,
                event.is_user_edit,
                event.ime_composition,
                event.transaction_id,
                event.change_count,
                event.writing_inserted_units,
                event.writing_deleted_units,
                writing_net(&event),
                event.writing_count_version,
            ],
        )?;
        debug_core_event(&event);
        Ok(())
    }
}

fn local_date(timestamp: i64) -> NaiveDate {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.date_naive())
        .unwrap_or_else(|| Local::now().date_naive())
}

fn local_midnight_timestamp(date: NaiveDate) -> Option<i64> {
    let local_midnight = date.and_hms_opt(0, 0, 0)?;
    match Local.from_local_datetime(&local_midnight) {
        LocalResult::Single(value) => Some(value.timestamp()),
        LocalResult::Ambiguous(earliest, _) => Some(earliest.timestamp()),
        LocalResult::None => None,
    }
}

fn add_daily_net(totals: &mut HashMap<NaiveDate, (i64, i64)>, date: NaiveDate, net: i64) {
    if let Some((writing_net_units, _)) = totals.get_mut(&date) {
        *writing_net_units += net;
    }
}

fn add_active_interval(totals: &mut HashMap<NaiveDate, (i64, i64)>, start: i64, end: i64) {
    let mut cursor = start;
    while cursor < end {
        let date = local_date(cursor);
        let next_day = date
            .checked_add_signed(ChronoDuration::days(1))
            .and_then(local_midnight_timestamp)
            .filter(|boundary| *boundary > cursor);
        let segment_end = next_day.map_or(end, |boundary| end.min(boundary));
        if let Some((_, active_seconds)) = totals.get_mut(&date) {
            *active_seconds += segment_end.saturating_sub(cursor);
        }
        if segment_end <= cursor {
            break;
        }
        cursor = segment_end;
    }
}

fn writing_net(event: &EditEvent) -> Option<i64> {
    event
        .writing_inserted_units
        .zip(event.writing_deleted_units)
        .map(|(inserted, deleted)| inserted - deleted)
}

fn add_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    left.zip(right).map(|(left, right)| left + right)
}

fn matching_writing_version(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    left.zip(right)
        .filter(|(left, right)| left == right)
        .map(|(left, _)| left)
}

#[cfg(debug_assertions)]
fn debug_core_event(event: &EditEvent) {
    eprintln!(
        "[CORE EVENT] event_id={} +{} -{} source={}",
        event.transaction_id, event.inserted_chars, event.deleted_chars, event.source,
    );
}

#[cfg(not(debug_assertions))]
fn debug_core_event(_event: &EditEvent) {}

#[cfg(test)]
mod tests {
    use chrono::Datelike;

    use super::*;

    fn config() -> CoreConfig {
        CoreConfig {
            idle_seconds: 10,
            session_end_seconds: 60,
            source: "test",
        }
    }

    fn event(
        document_id: &str,
        timestamp_ms: i64,
        source: &str,
        origin: &str,
        inserted: i64,
        deleted: i64,
    ) -> EditEvent {
        EditEvent {
            document_id: document_id.to_string(),
            timestamp_ms,
            source: source.to_string(),
            tracking_origin: origin.to_string(),
            inserted_chars: inserted,
            deleted_chars: deleted,
            writing_inserted_units: Some(inserted),
            writing_deleted_units: Some(deleted),
            writing_count_version: Some(WRITING_COUNT_VERSION),
            is_user_edit: true,
            ime_composition: false,
            transaction_id: Uuid::new_v4().to_string(),
            change_count: 1,
        }
    }

    fn snapshot() -> DocumentSnapshot {
        DocumentSnapshot {
            document_id: String::new(),
            path: "test.md".to_string(),
            content: "初始正文".to_string(),
            hash: "hash".to_string(),
            mtime_ms: 0,
            size: 12,
            recovery_path: None,
        }
    }

    fn local_timestamp(date: NaiveDate, hour: u32, minute: u32, second: u32) -> i64 {
        Local
            .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, second)
            .single()
            .expect("valid local timestamp")
            .timestamp()
    }

    fn insert_document(database: &Storage, id: &str, path: &str) {
        database
            .conn
            .execute(
                "INSERT INTO documents (id, path, created_at, updated_at, last_hash, last_mtime_ms, last_size)
                 VALUES (?1, ?2, 0, 0, '', 0, 0)",
                params![id, path],
            )
            .expect("document");
    }

    #[test]
    fn unicode_scalar_counting_is_explicit_at_the_boundary() {
        assert_eq!("欧林索斯".chars().count(), 4);
        assert_eq!("🙂".chars().count(), 1);
        assert_eq!("👩‍🚀".chars().count(), 3);
    }

    #[test]
    fn runtime_timing_defaults_and_debug_overrides_are_validated() {
        let mut config = CoreConfig::new(300, 3600).expect("production defaults");
        assert_eq!(config.idle_seconds, 300);
        assert_eq!(config.session_end_seconds, 3600);

        config
            .apply_debug_timing(10, 60)
            .expect("valid development override");
        assert_eq!(config.idle_seconds, 10);
        assert_eq!(config.session_end_seconds, 60);

        assert!(CoreConfig::new(0, 60).is_err());
        assert!(CoreConfig::new(60, 10).is_err());
    }

    #[test]
    fn debug_timing_override_recomputes_open_session_immediately() {
        let mut core = WritingCore::new(
            PathBuf::from(":memory:"),
            CoreConfig::new(300, 3600).expect("production defaults"),
        )
        .expect("core");
        let document_id = core.open_document(&snapshot()).expect("open");
        core.record_edit_at(
            event(&document_id, 1_000, "file.diff", "file_diff", 1, 0),
            1,
        )
        .expect("edit");
        let before = core.status_at(31).expect("status before override");
        assert_eq!(before.state, "active");
        assert_eq!(before.session_active_seconds, 30);
        assert_eq!(before.session_idle_seconds, 0);

        core.apply_debug_timing(10, 60).expect("override");
        let after = core.status_at(31).expect("status after override");
        assert_eq!(after.state, "idle");
        assert_eq!(after.session_active_seconds, 10);
        assert_eq!(after.session_idle_seconds, 20);
        assert_eq!(after.since_last_edit_seconds, 30);
    }

    #[test]
    fn active_and_idle_intervals_are_accumulated_separately() {
        let database = Storage::open(Path::new(":memory:")).expect("database");
        database
            .conn
            .execute(
                "INSERT INTO documents (id, path, created_at, updated_at, last_hash, last_mtime_ms, last_size)
                 VALUES ('doc', 'test.md', 0, 0, '', 0, 0)",
                [],
            )
            .expect("document");
        database
            .record_event(
                "doc",
                event("doc", 1_000, "input.type", "editor_transaction", 4, 0),
                &config(),
            )
            .expect("first event");
        database
            .record_event(
                "doc",
                event("doc", 31_000, "file.diff", "file_diff", 1, 0),
                &config(),
            )
            .expect("second event");
        let session = database
            .latest_session("doc")
            .expect("query")
            .expect("session");
        assert_eq!(session.active_seconds, 10);
        assert_eq!(session.idle_seconds, 20);
        assert_eq!(session.tracking_origin, "mixed");
    }

    #[test]
    fn status_keeps_idle_history_after_writing_resumes() {
        let mut core = WritingCore::new(PathBuf::from(":memory:"), config()).expect("core");
        let document_id = core.open_document(&snapshot()).expect("open");
        core.record_edit_at(
            event(&document_id, 1_000, "file.diff", "file_diff", 1, 0),
            1,
        )
        .expect("first edit");

        let idle_status = core.status_at(31).expect("idle status");
        assert_eq!(idle_status.state, "idle");
        assert_eq!(idle_status.session_span_seconds, 30);
        assert_eq!(idle_status.session_active_seconds, 10);
        assert_eq!(idle_status.session_idle_seconds, 20);
        assert_eq!(idle_status.since_last_edit_seconds, 30);

        core.record_edit_at(
            event(&document_id, 31_000, "file.diff", "file_diff", 1, 0),
            31,
        )
        .expect("resume edit");
        let active_status = core.status_at(32).expect("active status");
        assert_eq!(active_status.state, "active");
        assert_eq!(active_status.session_active_seconds, 11);
        assert_eq!(active_status.session_idle_seconds, 20);
        assert_eq!(active_status.since_last_edit_seconds, 1);
        assert_eq!(
            active_status.session_span_seconds,
            active_status.session_active_seconds + active_status.session_idle_seconds
        );
        assert_eq!(
            active_status.document_id.as_deref(),
            Some(document_id.as_str())
        );
        assert_eq!(
            idle_status.session_id, active_status.session_id,
            "resuming in the same session must preserve session identity"
        );
    }

    #[test]
    fn cumulative_character_statistics_are_preserved_when_a_new_diff_arrives() {
        let mut core = WritingCore::new(PathBuf::from(":memory:"), config()).expect("core");
        let document_id = core.open_document(&snapshot()).expect("open");
        core.record_edit_at(
            event(&document_id, 1_000, "file.diff", "file_diff", 303, 273),
            1,
        )
        .expect("initial accumulated diff");

        let status = core
            .record_edit_at(
                event(&document_id, 2_000, "file.diff", "file_diff", 7, 0),
                2,
            )
            .expect("append diff");

        assert_eq!(status.session_inserted_chars, 310);
        assert_eq!(status.session_deleted_chars, 273);
        assert_eq!(status.session_net_chars, 37);
    }

    #[test]
    fn writing_statistics_are_separate_from_raw_scalars_and_document_count_is_current() {
        let mut core = WritingCore::new(PathBuf::from(":memory:"), config()).expect("core");
        let document_id = core.open_document(&snapshot()).expect("open");
        let waiting = core.status_at(0).expect("waiting status");
        assert_eq!(waiting.document_writing_count, Some(4));

        let status = core
            .record_edit_at(
                EditEvent {
                    document_id,
                    timestamp_ms: 1_000,
                    source: "file.diff".to_string(),
                    tracking_origin: "file_diff".to_string(),
                    inserted_chars: 1,
                    deleted_chars: 1,
                    writing_inserted_units: Some(0),
                    writing_deleted_units: Some(0),
                    writing_count_version: Some(WRITING_COUNT_VERSION),
                    is_user_edit: true,
                    ime_composition: false,
                    transaction_id: Uuid::new_v4().to_string(),
                    change_count: 2,
                },
                1,
            )
            .expect("record punctuation-only rewrite");

        assert_eq!(status.session_inserted_chars, 1);
        assert_eq!(status.session_deleted_chars, 1);
        assert_eq!(status.session_net_chars, 0);
        assert_eq!(status.session_writing_inserted_units, Some(0));
        assert_eq!(status.session_writing_deleted_units, Some(0));
        assert_eq!(status.session_writing_net_units, Some(0));
        assert_eq!(
            status.session_writing_count_version,
            Some(WRITING_COUNT_VERSION)
        );
    }

    #[test]
    fn live_snapshot_updates_writing_count_without_replacing_the_disk_save_hash() {
        let mut core = WritingCore::new(PathBuf::from(":memory:"), config()).expect("core");
        let snapshot = snapshot();
        let document_id = core.open_document(&snapshot).expect("open");

        core.mark_live_snapshot("初始正文新增")
            .expect("live snapshot");
        let status = core.status_at(0).expect("status");
        assert_eq!(status.document_writing_count, Some(6));
        core.ensure_save_is_safe(&document_id, Path::new("test.md"), "hash")
            .expect("live monitoring must not change the on-disk save baseline");
    }

    #[test]
    fn repeated_event_id_is_persisted_only_once() {
        let database = Storage::open(Path::new(":memory:")).expect("database");
        database
            .conn
            .execute(
                "INSERT INTO documents (id, path, created_at, updated_at, last_hash, last_mtime_ms, last_size)
                 VALUES ('doc', 'test.md', 0, 0, '', 0, 0)",
                [],
            )
            .expect("document");
        let first = event("doc", 1_000, "file.diff", "file_diff", 7, 0);
        database
            .record_event("doc", first.clone(), &config())
            .expect("first event");
        database
            .record_event("doc", first, &config())
            .expect("duplicate event");

        let count: i64 = database
            .conn
            .query_row("SELECT COUNT(*) FROM edit_events", [], |row| row.get(0))
            .expect("event count");
        let session = database
            .latest_session("doc")
            .expect("query")
            .expect("session");
        assert_eq!(count, 1);
        assert_eq!(session.inserted_chars, 7);
        assert_eq!(session.net_chars, 7);
    }

    #[test]
    fn session_end_closes_after_the_end_threshold_and_starts_new_session() {
        let database = Storage::open(Path::new(":memory:")).expect("database");
        database
            .conn
            .execute(
                "INSERT INTO documents (id, path, created_at, updated_at, last_hash, last_mtime_ms, last_size)
                 VALUES ('doc', 'test.md', 0, 0, '', 0, 0)",
                [],
            )
            .expect("document");
        database
            .record_event(
                "doc",
                event("doc", 1_000, "input.type", "editor_transaction", 1, 0),
                &config(),
            )
            .expect("first event");
        database
            .record_event(
                "doc",
                event("doc", 62_000, "file.diff", "file_diff", 2, 0),
                &config(),
            )
            .expect("second event");
        let ended: i64 = database
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE status = 'ended' AND ended_at = 61",
                [],
                |row| row.get(0),
            )
            .expect("ended session");
        assert_eq!(ended, 1);
        let current = database
            .latest_session("doc")
            .expect("query")
            .expect("session");
        assert_eq!(current.inserted_chars, 2);
        assert_eq!(current.status, "active");
    }

    #[test]
    fn untrack_close_is_a_noop_without_a_document_or_open_session() {
        let mut core = WritingCore::new(PathBuf::from(":memory:"), config()).expect("core");
        assert!(!core.end_current_session_at(1).expect("no document"));

        let document_id = core.open_document(&snapshot()).expect("open");
        assert!(!core.end_current_session_at(2).expect("no session"));
        core.record_edit_at(
            event(&document_id, 1_000, "file.diff", "file_diff", 1, 0),
            1,
        )
        .expect("edit");
        assert!(core.end_current_session_at(2).expect("close open session"));
        assert!(!core.end_current_session_at(3).expect("already closed"));
    }

    #[test]
    fn untrack_close_uses_the_automatic_boundary_and_the_next_edit_restarts_it() {
        let mut core = WritingCore::new(PathBuf::from(":memory:"), config()).expect("core");
        let document_id = core.open_document(&snapshot()).expect("open");
        let first = core
            .record_edit_at(
                event(&document_id, 1_000, "file.diff", "file_diff", 3, 0),
                1,
            )
            .expect("first edit");
        let session_id = first.session_id.clone().expect("session id");

        assert!(core.end_current_session_at(31).expect("close on untrack"));
        core.close_document();
        core.open_document(&snapshot()).expect("re-track document");
        let retained = core.status_at(32).expect("status after re-track");
        assert_eq!(retained.state, "ended");
        assert_eq!(retained.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(retained.session_span_seconds, 30);
        assert_eq!(retained.session_active_seconds, 10);
        assert_eq!(retained.session_idle_seconds, 20);

        let next = core
            .record_edit_at(
                event(&document_id, 33_000, "file.diff", "file_diff", 2, 0),
                33,
            )
            .expect("next edit");
        assert_ne!(next.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(next.state, "active");
        assert_eq!(next.session_writing_net_units, Some(2));
        assert_eq!(next.session_span_seconds, 0);
    }

    #[test]
    fn daily_stats_are_seven_contiguous_local_days_with_negative_net_and_zero_fill() {
        let database = Storage::open(Path::new(":memory:")).expect("database");
        insert_document(&database, "doc-a", "a.md");
        insert_document(&database, "doc-b", "b.md");
        let today = Local::now().date_naive();
        let now = local_timestamp(today, 0, 5, 0);
        let old = local_timestamp(today - ChronoDuration::days(2), 12, 0, 0);
        let current = local_timestamp(today, 0, 0, 1);

        database
            .record_event(
                "doc-a",
                event("doc-a", old * 1_000, "file.diff", "file_diff", 7, 0),
                &config(),
            )
            .expect("old document event");
        database
            .record_event(
                "doc-b",
                event(
                    "doc-b",
                    current * 1_000,
                    "editor.snapshot",
                    "editor_snapshot",
                    2,
                    5,
                ),
                &config(),
            )
            .expect("current document event");

        let stats = database
            .daily_stats_at(now, 7, &config())
            .expect("daily stats");
        assert_eq!(stats.schema_version, 1);
        assert_eq!(stats.days.len(), 7);
        assert_eq!(
            stats.days.first().unwrap().date,
            (today - ChronoDuration::days(6))
                .format("%Y-%m-%d")
                .to_string()
        );
        assert_eq!(
            stats.days.last().unwrap().date,
            today.format("%Y-%m-%d").to_string()
        );
        assert_eq!(stats.days[4].writing_net_units, 7);
        assert_eq!(stats.days[6].writing_net_units, -3);
        assert_eq!(stats.days[4].active_seconds, 10);
        assert_eq!(stats.days[6].active_seconds, 10);
        assert!(stats
            .days
            .iter()
            .any(|day| day.writing_net_units == 0 && day.active_seconds == 0));
    }

    #[test]
    fn daily_active_time_splits_a_session_at_local_midnight_and_includes_open_tail() {
        let database = Storage::open(Path::new(":memory:")).expect("database");
        insert_document(&database, "doc", "test.md");
        let today = Local::now().date_naive();
        let yesterday = today - ChronoDuration::days(1);
        let first = local_timestamp(yesterday, 23, 59, 55);
        let second = local_timestamp(today, 0, 0, 20);
        let now = local_timestamp(today, 0, 1, 0);

        database
            .record_event(
                "doc",
                event("doc", first * 1_000, "file.diff", "file_diff", 1, 0),
                &config(),
            )
            .expect("first event");
        database
            .record_event(
                "doc",
                event("doc", second * 1_000, "file.diff", "file_diff", 2, 0),
                &config(),
            )
            .expect("second event");
        let session = database
            .latest_session("doc")
            .expect("session query")
            .expect("session");
        database
            .end_session(&session.id, local_timestamp(today, 0, 0, 30), 20, 0)
            .expect("manual end");

        let stats = database
            .daily_stats_at(now, 2, &config())
            .expect("daily stats");
        assert_eq!(stats.days[0].writing_net_units, 1);
        assert_eq!(stats.days[1].writing_net_units, 2);
        assert_eq!(stats.days[0].active_seconds, 5);
        assert_eq!(stats.days[1].active_seconds, 15);
        assert_eq!(
            stats.days[0].active_seconds + stats.days[1].active_seconds,
            20
        );
    }

    #[test]
    fn reopening_core_keeps_open_session_for_session_continuity() {
        let database_path = std::env::temp_dir().join(format!(
            "caelorum-writing-core-reopen-{}.sqlite",
            Uuid::new_v4()
        ));
        let session_id = {
            let mut core = WritingCore::new(database_path.clone(), config()).expect("core");
            let document_id = core.open_document(&snapshot()).expect("open");
            core.record_edit_at(
                event(&document_id, 1_000, "file.diff", "file_diff", 1, 0),
                1,
            )
            .expect("first event");
            core.status_at(2).expect("status").session_id.unwrap()
        };

        let reopened_status = {
            let mut core =
                WritingCore::new(database_path.clone(), config()).expect("reopened core");
            core.open_document(&snapshot()).expect("reopen document");
            core.status_at(3).expect("reopened status")
        };

        assert_eq!(
            reopened_status.session_id.as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(reopened_status.state, "active");
        assert_eq!(reopened_status.session_writing_net_units, Some(1));

        let _ = std::fs::remove_file(&database_path);
        let _ = std::fs::remove_file(database_path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(database_path.with_extension("sqlite-shm"));
    }
}
