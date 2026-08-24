mod core;
mod diff;
mod status_api;
mod writing;

use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use core::{
    epoch_millis, CoreConfig, CoreStatus, DocumentSnapshot, EditEvent, RuntimeTimingConfig,
    WritingCore,
};
use diff::{summarize, DiffSummary};
use notify::{
    event::{EventKind, ModifyKind},
    Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use status_api::StatusApiRuntime;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use uuid::Uuid;
use writing::{writing_delta, WritingDelta};

const ALLOWED_EXTENSIONS: [&str; 2] = ["md", "markdown"];
const FILE_DIFF_DEBOUNCE: Duration = Duration::from_millis(300);
const FILE_RETRY_DELAY: Duration = Duration::from_millis(150);
const MAX_LIVE_SNAPSHOT_BYTES: usize = 1024 * 1024;

struct AppState {
    core: Mutex<WritingCore>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    watch: Mutex<WatchState>,
    file_processing: Mutex<()>,
    service_instance_id: String,
    revision: Mutex<RevisionTracker>,
    status_api: Mutex<Option<StatusApiRuntime>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusFingerprint {
    tracking: bool,
    document_id: Option<String>,
    activity_state: String,
    session_id: Option<String>,
    session_started_at: Option<i64>,
    document_writing_count: Option<i64>,
    session_inserted_chars: i64,
    session_deleted_chars: i64,
    session_net_chars: i64,
    session_writing_inserted_units: Option<i64>,
    session_writing_deleted_units: Option<i64>,
    session_writing_net_units: Option<i64>,
}

impl StatusFingerprint {
    fn inactive() -> Self {
        Self {
            tracking: false,
            document_id: None,
            activity_state: "inactive".to_string(),
            session_id: None,
            session_started_at: None,
            document_writing_count: None,
            session_inserted_chars: 0,
            session_deleted_chars: 0,
            session_net_chars: 0,
            session_writing_inserted_units: None,
            session_writing_deleted_units: None,
            session_writing_net_units: None,
        }
    }

    fn from_status(status: &CoreStatus, tracking: bool) -> Self {
        Self {
            tracking,
            document_id: tracking.then(|| status.document_id.clone()).flatten(),
            activity_state: activity_state(tracking, status),
            session_id: tracking.then(|| status.session_id.clone()).flatten(),
            session_started_at: tracking.then(|| status.session_started_at).flatten(),
            document_writing_count: tracking.then_some(status.document_writing_count).flatten(),
            session_inserted_chars: if tracking {
                status.session_inserted_chars
            } else {
                0
            },
            session_deleted_chars: if tracking {
                status.session_deleted_chars
            } else {
                0
            },
            session_net_chars: if tracking {
                status.session_net_chars
            } else {
                0
            },
            session_writing_inserted_units: tracking
                .then(|| status.session_writing_inserted_units)
                .flatten(),
            session_writing_deleted_units: tracking
                .then(|| status.session_writing_deleted_units)
                .flatten(),
            session_writing_net_units: tracking.then(|| status.session_writing_net_units).flatten(),
        }
    }
}

struct RevisionTracker {
    revision: u64,
    fingerprint: StatusFingerprint,
}

impl RevisionTracker {
    fn new() -> Self {
        Self {
            revision: 1,
            fingerprint: StatusFingerprint::inactive(),
        }
    }

    fn observe(&mut self, fingerprint: StatusFingerprint) -> u64 {
        if self.fingerprint != fingerprint {
            self.revision = self.revision.saturating_add(1);
            self.fingerprint = fingerprint;
        }
        self.revision
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchMode {
    EditorNotice,
    FileDiff,
}

#[derive(Debug, Clone)]
struct MemorySnapshot {
    content: String,
    hash: String,
    mtime_ms: i64,
    size: u64,
}

fn file_diff_summary(previous: &MemorySnapshot, next: &MemorySnapshot) -> Option<DiffSummary> {
    if previous.hash == next.hash {
        return None;
    }
    let summary = summarize(&previous.content, &next.content);
    (summary.inserted_chars > 0 || summary.deleted_chars > 0).then_some(summary)
}

#[derive(Debug)]
enum LiveSnapshotDecision {
    StaleRevision,
    SameSnapshot,
    Changed(DiffSummary),
}

fn live_snapshot_decision(
    previous: &MemorySnapshot,
    last_editor_revision: Option<u64>,
    editor_revision: u64,
    next: &MemorySnapshot,
) -> Result<LiveSnapshotDecision, String> {
    if last_editor_revision.is_some_and(|last| editor_revision <= last) {
        return Ok(LiveSnapshotDecision::StaleRevision);
    }
    if previous.hash == next.hash {
        return Ok(LiveSnapshotDecision::SameSnapshot);
    }
    file_diff_summary(previous, next)
        .map(LiveSnapshotDecision::Changed)
        .ok_or_else(|| "snapshot changed hash without a text diff".to_string())
}

struct WatchState {
    path: Option<PathBuf>,
    document_id: Option<String>,
    mode: Option<WatchMode>,
    last_good_snapshot: Option<MemorySnapshot>,
    last_good_hash: Option<String>,
    expected_hash: Option<String>,
    pending_atomic_replace: bool,
    last_external_notice: Option<Instant>,
    editor_revisions: HashMap<String, u64>,
    generation: u64,
}

impl Default for WatchState {
    fn default() -> Self {
        Self {
            path: None,
            document_id: None,
            mode: None,
            last_good_snapshot: None,
            last_good_hash: None,
            expected_hash: None,
            pending_atomic_replace: false,
            last_external_notice: None,
            editor_revisions: HashMap::new(),
            generation: 0,
        }
    }
}

fn accept_last_good_snapshot(watch: &mut WatchState, snapshot: MemorySnapshot) {
    watch.last_good_hash = Some(snapshot.hash.clone());
    watch.last_good_snapshot = Some(snapshot);
    watch.pending_atomic_replace = false;
}

fn mark_target_temporarily_missing(watch: &mut WatchState) {
    watch.pending_atomic_replace = true;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct MonitorDocument {
    document_id: String,
    path: String,
    hash: String,
    mtime_ms: i64,
    size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct TrackingChangedPayload {
    document: Option<MonitorDocument>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct TrackApiRequest {
    path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct LiveSnapshotApiRequest {
    path: String,
    content: String,
    source: String,
    editor_instance_id: String,
    editor_revision: u64,
    #[serde(default)]
    timestamp_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct LiveSnapshotApiResponse {
    accepted: bool,
    reason: Option<String>,
    status: StatusApiResponse,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct ExternalModification {
    path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct FileDiffPayload {
    path: String,
    event: EditEvent,
    status: CoreStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct WatchErrorPayload {
    path: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct SaveResult {
    path: String,
    hash: String,
    mtime_ms: i64,
    size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct WindowCapability {
    backend: String,
    supported: bool,
    reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct StoredWindowState {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    prev_x: i32,
    prev_y: i32,
    maximized: bool,
    visible: bool,
    decorated: bool,
    fullscreen: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct WindowStateDiagnostics {
    backend: String,
    label: String,
    current_width: u32,
    current_height: u32,
    current_x: i32,
    current_y: i32,
    state_filename: String,
    state_path: String,
    stored_state_exists: bool,
    stored_state: Option<StoredWindowState>,
    stored_state_parse_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct WatchDiagnostics {
    path: Option<String>,
    document_id: Option<String>,
    baseline_chars: Option<usize>,
    current_file_chars: Option<usize>,
    last_good_hash_prefix: Option<String>,
    watcher_state: String,
    pending_atomic_replace: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct StatusApiResponse {
    schema_version: u8,
    service_instance_id: String,
    revision: u64,
    tracking: bool,
    activity_state: String,
    document: Option<StatusApiDocument>,
    session: Option<StatusApiSession>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct StatusApiDocument {
    id: String,
    name: String,
    path_hash: String,
    writing_count: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct StatusApiSession {
    id: String,
    started_at: Option<String>,
    span_seconds: i64,
    active_seconds: i64,
    idle_seconds: i64,
    since_last_edit_seconds: i64,
    writing_inserted_units: Option<i64>,
    writing_deleted_units: Option<i64>,
    writing_net_units: Option<i64>,
}

fn activity_state(tracking: bool, status: &CoreStatus) -> String {
    if !tracking || status.session_id.is_none() || status.state == "waiting" {
        "inactive".to_string()
    } else {
        match status.state.as_str() {
            "active" | "idle" | "ended" => status.state.clone(),
            _ => "inactive".to_string(),
        }
    }
}

fn format_rfc3339_utc(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;

    // Civil date conversion from days since 1970-01-01, using the proleptic
    // Gregorian calendar. The API deliberately emits UTC and has no timezone
    // dependency beyond the standard library.
    let shifted_days = days + 719_468;
    let era = if shifted_days >= 0 {
        shifted_days
    } else {
        shifted_days - 146_096
    } / 146_097;
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn status_api_response(
    service_instance_id: &str,
    revision: u64,
    tracking: bool,
    path: Option<&Path>,
    status: &CoreStatus,
) -> StatusApiResponse {
    let document = if tracking {
        status.document_id.clone().map(|id| StatusApiDocument {
            id,
            name: path
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string(),
            path_hash: path.map(path_identity_hash).unwrap_or_default(),
            writing_count: status.document_writing_count,
        })
    } else {
        None
    };

    let session = status.session_id.clone().map(|id| StatusApiSession {
        id,
        started_at: status.session_started_at.map(format_rfc3339_utc),
        span_seconds: status.session_span_seconds,
        active_seconds: status.session_active_seconds,
        idle_seconds: status.session_idle_seconds,
        since_last_edit_seconds: status.since_last_edit_seconds,
        writing_inserted_units: status.session_writing_inserted_units,
        writing_deleted_units: status.session_writing_deleted_units,
        writing_net_units: status.session_writing_net_units,
    });

    StatusApiResponse {
        schema_version: 1,
        service_instance_id: service_instance_id.to_string(),
        revision,
        tracking,
        activity_state: activity_state(tracking, status),
        document,
        session,
    }
}

fn path_identity_hash(path: &Path) -> String {
    file_hash(path.to_string_lossy().as_bytes())
}

fn mtime_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn validate_markdown_path(path: &Path) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .ok_or_else(|| "Only .md and .markdown files are supported".to_string())?;

    if ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
        Ok(())
    } else {
        Err("Only .md and .markdown files are supported".to_string())
    }
}

fn file_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn temporary_save_path(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Markdown path has no parent directory".to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Markdown path has no valid file name".to_string())?;
    Ok(parent.join(format!("{file_name}.writing-tmp")))
}

fn canonical_markdown_path(path: &Path) -> Result<PathBuf, String> {
    validate_markdown_path(path)?;
    path.canonicalize()
        .map_err(|error| format!("Cannot resolve Markdown path: {error}"))
}

fn read_memory_snapshot(path: &Path) -> Result<MemorySnapshot, String> {
    let bytes = fs::read(path).map_err(|error| format!("Cannot read Markdown file: {error}"))?;
    let content = String::from_utf8(bytes.clone())
        .map_err(|_| "The Markdown file is not valid UTF-8".to_string())?;
    let metadata =
        fs::metadata(path).map_err(|error| format!("Cannot stat Markdown file: {error}"))?;
    Ok(MemorySnapshot {
        content,
        hash: file_hash(&bytes),
        mtime_ms: mtime_ms(&metadata),
        size: metadata.len(),
    })
}

fn read_document(path: &Path) -> Result<DocumentSnapshot, String> {
    let path = canonical_markdown_path(path)?;
    let memory = read_memory_snapshot(&path)?;
    let recovery_path = temporary_save_path(&path)
        .ok()
        .filter(|temporary_path| temporary_path.exists())
        .map(|temporary_path| temporary_path.to_string_lossy().to_string());

    Ok(DocumentSnapshot {
        document_id: String::new(),
        path: path.to_string_lossy().to_string(),
        content: memory.content,
        hash: memory.hash,
        mtime_ms: memory.mtime_ms,
        size: memory.size,
        recovery_path,
    })
}

fn atomic_save(path: &Path, content: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Markdown path has no parent directory".to_string())?;
    let temporary_path = temporary_save_path(path)?;

    let write_result = (|| -> Result<(), String> {
        let mut temporary = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary_path)
            .map_err(|error| format!("Cannot create temporary save file: {error}"))?;
        temporary
            .write_all(content.as_bytes())
            .map_err(|error| format!("Cannot write temporary save file: {error}"))?;
        temporary
            .sync_all()
            .map_err(|error| format!("Cannot flush temporary save file: {error}"))?;
        fs::rename(&temporary_path, path)
            .map_err(|error| format!("Cannot atomically replace Markdown file: {error}"))?;

        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if write_result.is_err() {
        // Keeping this file is intentional: it is a recoverable artifact for
        // the debug spike and never replaces the original on a failed write.
        let _ = fs::metadata(&temporary_path);
    }

    write_result
}

fn current_hash(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("Cannot check current file: {error}"))?;
    Ok(file_hash(&bytes))
}

fn reset_tracking_state(state: &AppState) -> Result<(), String> {
    *state
        .watcher
        .lock()
        .map_err(|_| "Watcher lock is poisoned".to_string())? = None;
    let mut watch = state
        .watch
        .lock()
        .map_err(|_| "Watch state lock is poisoned".to_string())?;
    watch.generation = watch.generation.wrapping_add(1);
    watch.path = None;
    watch.document_id = None;
    watch.mode = None;
    watch.last_good_snapshot = None;
    watch.last_good_hash = None;
    watch.expected_hash = None;
    watch.pending_atomic_replace = false;
    watch.last_external_notice = None;
    watch.editor_revisions.clear();
    drop(watch);
    state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?
        .close_document();
    Ok(())
}

fn tracking_path(state: &AppState) -> Result<Option<PathBuf>, String> {
    state
        .watch
        .lock()
        .map_err(|_| "Watch state lock is poisoned".to_string())
        .map(|watch| watch.path.clone())
}

fn sync_status_revision(
    state: &AppState,
    status: &CoreStatus,
    tracking: bool,
) -> Result<u64, String> {
    let fingerprint = StatusFingerprint::from_status(status, tracking);
    state
        .revision
        .lock()
        .map_err(|_| "Status revision lock is poisoned".to_string())
        .map(|mut revision| revision.observe(fingerprint))
}

fn refresh_status_revision(state: &AppState) -> Result<u64, String> {
    let path = tracking_path(state)?;
    let tracking = path.is_some();
    let mut core = state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?;
    let status = core.status_read_only().map_err(|error| error.to_string())?;
    drop(core);
    sync_status_revision(state, &status, tracking)
}

fn read_status_api_response(app: &AppHandle) -> Result<StatusApiResponse, String> {
    let state = app.state::<AppState>();
    let path = tracking_path(state.inner())?;
    let tracking = path.is_some();
    let mut core = state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?;
    let status = core.status_read_only().map_err(|error| error.to_string())?;
    drop(core);

    let revision = sync_status_revision(state.inner(), &status, tracking)?;
    Ok(status_api_response(
        &state.service_instance_id,
        revision,
        tracking,
        path.as_deref(),
        &status,
    ))
}

pub(crate) fn read_status_api_json(app: &AppHandle) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&read_status_api_response(app)?).map_err(|error| error.to_string())
}

pub(crate) fn track_document_api_json(
    app: &AppHandle,
    request: TrackApiRequest,
) -> Result<Vec<u8>, String> {
    let state = app.state::<AppState>();
    let (snapshot, _) = start_tracking(request.path, WatchMode::FileDiff, state.inner(), app)?;
    let document = snapshot_to_monitor(&snapshot);
    let _ = app.emit(
        "tracking-changed",
        TrackingChangedPayload {
            document: Some(document),
        },
    );
    read_status_api_json(app)
}

fn valid_editor_instance_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn ingest_live_snapshot_api_json(
    app: &AppHandle,
    request: LiveSnapshotApiRequest,
) -> Result<Vec<u8>, String> {
    if request.source != "caelorum" {
        return Err("snapshot source must be caelorum".to_string());
    }
    if !valid_editor_instance_id(&request.editor_instance_id) {
        return Err("editor_instance_id is invalid".to_string());
    }
    if request.content.len() > MAX_LIVE_SNAPSHOT_BYTES {
        return Err("snapshot exceeds the 1 MiB limit".to_string());
    }
    let _client_timestamp_ms = request.timestamp_ms;
    let path = canonical_markdown_path(Path::new(&request.path))?;
    let state = app.state::<AppState>();
    let _processing = state
        .file_processing
        .lock()
        .map_err(|_| "File processing lock is poisoned".to_string())?;

    let (document_id, previous, last_editor_revision) = {
        let watch = state
            .watch
            .lock()
            .map_err(|_| "Watch state lock is poisoned".to_string())?;
        if watch.path.as_deref() != Some(path.as_path()) || watch.mode != Some(WatchMode::FileDiff)
        {
            return Err("tracked document does not match snapshot path".to_string());
        }
        let document_id = watch
            .document_id
            .clone()
            .ok_or_else(|| "tracked document is unavailable".to_string())?;
        let previous = watch
            .last_good_snapshot
            .clone()
            .ok_or_else(|| "tracked snapshot baseline is unavailable".to_string())?;
        let last_editor_revision = watch
            .editor_revisions
            .get(&request.editor_instance_id)
            .copied();
        (document_id, previous, last_editor_revision)
    };

    let next = MemorySnapshot {
        hash: file_hash(request.content.as_bytes()),
        size: request.content.len() as u64,
        mtime_ms: previous.mtime_ms,
        content: request.content,
    };
    let summary = match live_snapshot_decision(
        &previous,
        last_editor_revision,
        request.editor_revision,
        &next,
    )? {
        LiveSnapshotDecision::StaleRevision => {
            let response = LiveSnapshotApiResponse {
                accepted: false,
                reason: Some("stale_editor_revision".to_string()),
                status: read_status_api_response(app)?,
            };
            return serde_json::to_vec(&response).map_err(|error| error.to_string());
        }
        LiveSnapshotDecision::SameSnapshot => {
            if let Ok(mut watch) = state.watch.lock() {
                watch
                    .editor_revisions
                    .insert(request.editor_instance_id, request.editor_revision);
            }
            let response = LiveSnapshotApiResponse {
                accepted: false,
                reason: Some("same_snapshot".to_string()),
                status: read_status_api_response(app)?,
            };
            return serde_json::to_vec(&response).map_err(|error| error.to_string());
        }
        LiveSnapshotDecision::Changed(summary) => summary,
    };
    let event = EditEvent {
        document_id: document_id.clone(),
        timestamp_ms: epoch_millis(),
        source: "caelorum.snapshot".to_string(),
        tracking_origin: "editor_snapshot".to_string(),
        inserted_chars: summary.inserted_chars,
        deleted_chars: summary.deleted_chars,
        writing_inserted_units: Some(summary.writing.inserted_units),
        writing_deleted_units: Some(summary.writing.deleted_units),
        writing_count_version: Some(summary.writing.count_version),
        is_user_edit: true,
        ime_composition: false,
        transaction_id: format!(
            "caelorum-{}-{}",
            request.editor_instance_id, request.editor_revision
        ),
        change_count: summary.change_count,
    };

    let status = {
        let mut core = state
            .core
            .lock()
            .map_err(|_| "Writing Core lock is poisoned".to_string())?;
        core.record_edit(event).map_err(|error| error.to_string())?;
        core.mark_live_snapshot(&next.content)
            .map_err(|error| error.to_string())?;
        core.status().map_err(|error| error.to_string())?
    };

    {
        let mut watch = state
            .watch
            .lock()
            .map_err(|_| "Watch state lock is poisoned".to_string())?;
        if watch.path.as_deref() != Some(path.as_path())
            || watch.document_id.as_deref() != Some(document_id.as_str())
        {
            return Err("tracked document changed during snapshot ingest".to_string());
        }
        watch.expected_hash = Some(next.hash.clone());
        watch
            .editor_revisions
            .insert(request.editor_instance_id, request.editor_revision);
        accept_last_good_snapshot(&mut watch, next);
    }
    sync_status_revision(state.inner(), &status, true)?;
    let response = LiveSnapshotApiResponse {
        accepted: true,
        reason: None,
        status: read_status_api_response(app)?,
    };
    serde_json::to_vec(&response).map_err(|error| error.to_string())
}

fn shutdown_status_api(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(mut runtime) = state.status_api.lock() else {
        eprintln!("[STATUS API] shutdown skipped: status API lock is poisoned");
        return;
    };
    if let Some(mut runtime) = runtime.take() {
        runtime.shutdown();
        eprintln!("[STATUS API] stopped");
    }
}

fn configure_watcher(
    app: &AppHandle,
    path: &Path,
    document_id: &str,
    mode: WatchMode,
    snapshot: MemorySnapshot,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Markdown path has no parent directory".to_string())?
        .to_path_buf();
    let watched_path = path.to_path_buf();
    let callback_app = app.clone();

    let mut watcher = RecommendedWatcher::new(
        move |result| {
            if let Ok(event) = result {
                handle_file_event(&callback_app, &watched_path, event);
            }
        },
        NotifyConfig::default(),
    )
    .map_err(|error| format!("Cannot create file watcher: {error}"))?;

    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .map_err(|error| format!("Cannot watch Markdown directory: {error}"))?;

    let state = app.state::<AppState>();
    let generation = {
        let mut watch = state
            .watch
            .lock()
            .map_err(|_| "Watch state lock is poisoned".to_string())?;
        watch.generation = watch.generation.wrapping_add(1);
        watch.path = Some(path.to_path_buf());
        watch.document_id = Some(document_id.to_string());
        watch.mode = Some(mode);
        watch.last_good_hash = Some(snapshot.hash.clone());
        watch.expected_hash = Some(snapshot.hash.clone());
        watch.last_good_snapshot = Some(snapshot);
        watch.last_external_notice = None;
        watch.editor_revisions.clear();
        watch.pending_atomic_replace = false;
        watch.generation
    };
    let _ = generation;
    *state
        .watcher
        .lock()
        .map_err(|_| "Watcher lock is poisoned".to_string())? = Some(watcher);
    Ok(())
}

fn event_targets_path(event: &Event, path: &Path) -> bool {
    event.paths.iter().any(|event_path| {
        event_path == path
            || event_path
                .file_name()
                .zip(path.file_name())
                .is_some_and(|(left, right)| left == right)
    })
}

fn is_processable_file_event(event: &Event) -> bool {
    !matches!(event.kind, EventKind::Access(_))
}

fn handle_file_event(app: &AppHandle, path: &Path, event: Event) {
    if !event_targets_path(&event, path) || !is_processable_file_event(&event) {
        return;
    }
    debug_watch_raw_event(path, &event);

    let state = app.state::<AppState>();
    let target_may_be_replaced = target_may_be_replaced(&event);
    let generation = {
        let Ok(mut watch) = state.watch.lock() else {
            return;
        };
        if watch.path.as_deref() != Some(path) || watch.mode.is_none() {
            return;
        }
        if target_may_be_replaced {
            mark_target_temporarily_missing(&mut watch);
            eprintln!(
                "[WATCH] target temporarily missing or being replaced path={} baseline_retained=true",
                path.to_string_lossy()
            );
        }
        watch.generation = watch.generation.wrapping_add(1);
        watch.generation
    };
    let callback_app = app.clone();
    let watched_path = path.to_path_buf();
    thread::spawn(move || {
        thread::sleep(FILE_DIFF_DEBOUNCE);
        for _ in 0..8 {
            if !is_current_generation(&callback_app, &watched_path, generation) {
                return;
            }
            match read_memory_snapshot(&watched_path) {
                Ok(snapshot) => {
                    process_file_snapshot(&callback_app, &watched_path, generation, snapshot);
                    return;
                }
                Err(error) if error.contains("No such file") => {
                    thread::sleep(FILE_RETRY_DELAY);
                }
                Err(error) => {
                    emit_watch_error(&callback_app, &watched_path, error);
                    return;
                }
            }
        }
        eprintln!(
            "[WATCH] target still missing after retry window path={} baseline_retained=true",
            watched_path.to_string_lossy()
        );
    });
}

fn target_may_be_replaced(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

#[cfg(debug_assertions)]
fn debug_watch_raw_event(path: &Path, event: &Event) {
    eprintln!(
        "[WATCH RAW] target={} kind={:?} paths={:?}",
        path.to_string_lossy(),
        event.kind,
        event.paths,
    );
}

#[cfg(not(debug_assertions))]
fn debug_watch_raw_event(_path: &Path, _event: &Event) {}

fn is_current_generation(app: &AppHandle, path: &Path, generation: u64) -> bool {
    let state = app.state::<AppState>();
    let Ok(watch) = state.watch.lock() else {
        return false;
    };
    watch.path.as_deref() == Some(path) && watch.generation == generation
}

fn process_file_snapshot(app: &AppHandle, path: &Path, generation: u64, next: MemorySnapshot) {
    let state = app.state::<AppState>();
    let Ok(_processing) = state.file_processing.lock() else {
        return;
    };
    let (mode, document_id, previous, last_good_hash, expected_hash) = {
        let Ok(watch) = state.watch.lock() else {
            return;
        };
        if watch.path.as_deref() != Some(path) || watch.generation != generation {
            return;
        }
        let Some(mode) = watch.mode else {
            return;
        };
        let Some(document_id) = watch.document_id.clone() else {
            return;
        };
        let Some(previous) = watch.last_good_snapshot.clone() else {
            return;
        };
        (
            mode,
            document_id,
            previous,
            watch.last_good_hash.clone(),
            watch.expected_hash.clone(),
        )
    };

    if Some(next.hash.clone()) == last_good_hash || Some(next.hash.clone()) == expected_hash {
        let next_hash = next.hash.clone();
        if let Ok(mut core) = state.core.lock() {
            if let Err(error) =
                core.mark_observed_snapshot(&next.hash, next.mtime_ms, next.size, &next.content)
            {
                emit_watch_error(app, path, error.to_string());
                return;
            }
        } else {
            emit_watch_error(app, path, "Writing Core lock is poisoned".to_string());
            return;
        }
        if let Ok(mut watch) = state.watch.lock() {
            if watch.path.as_deref() == Some(path) {
                accept_last_good_snapshot(&mut watch, next);
            }
        }
        eprintln!(
            "[WATCH] accepted stable snapshot path={} hash_prefix={} edit_event=false pending_atomic_replace=false",
            path.to_string_lossy(),
            &next_hash[..next_hash.len().min(12)]
        );
        return;
    }

    if mode == WatchMode::EditorNotice {
        let should_emit = state
            .watch
            .lock()
            .ok()
            .map(|mut watch| {
                let recent = watch
                    .last_external_notice
                    .is_some_and(|last| last.elapsed() < Duration::from_millis(500));
                if !recent {
                    watch.last_external_notice = Some(Instant::now());
                }
                !recent
            })
            .unwrap_or(false);
        if should_emit {
            let _ = app.emit(
                "external-modification",
                ExternalModification {
                    path: path.to_string_lossy().to_string(),
                },
            );
        }
        return;
    }

    let Some(summary) = file_diff_summary(&previous, &next) else {
        if let Ok(mut watch) = state.watch.lock() {
            if watch.path.as_deref() == Some(path) {
                accept_last_good_snapshot(&mut watch, next);
            }
        }
        return;
    };

    let event = EditEvent {
        document_id: document_id.clone(),
        timestamp_ms: epoch_millis(),
        source: "file.diff".to_string(),
        tracking_origin: "file_diff".to_string(),
        inserted_chars: summary.inserted_chars,
        deleted_chars: summary.deleted_chars,
        writing_inserted_units: Some(summary.writing.inserted_units),
        writing_deleted_units: Some(summary.writing.deleted_units),
        writing_count_version: Some(summary.writing.count_version),
        is_user_edit: true,
        ime_composition: false,
        transaction_id: format!("file-diff-{}-{}", epoch_millis(), Uuid::new_v4()),
        change_count: summary.change_count,
    };

    let status = {
        let Ok(mut core) = state.core.lock() else {
            emit_watch_error(app, path, "Writing Core lock is poisoned".to_string());
            return;
        };
        if let Err(error) = core.record_edit(event.clone()) {
            emit_watch_error(app, path, error.to_string());
            return;
        }
        if let Err(error) =
            core.mark_observed_snapshot(&next.hash, next.mtime_ms, next.size, &next.content)
        {
            emit_watch_error(app, path, error.to_string());
            return;
        }
        match core.status() {
            Ok(status) => status,
            Err(error) => {
                emit_watch_error(app, path, error.to_string());
                return;
            }
        }
    };

    if let Ok(mut watch) = state.watch.lock() {
        if watch.path.as_deref() == Some(path)
            && watch.last_good_hash.as_deref() == Some(previous.hash.as_str())
        {
            watch.expected_hash = Some(next.hash.clone());
            accept_last_good_snapshot(&mut watch, next);
        }
    }
    if let Err(error) = sync_status_revision(state.inner(), &status, true) {
        eprintln!("[STATUS API] revision update failed after file diff: {error}");
    }
    debug_tauri_status("file-diff-event", &status);
    debug_tauri_event("file-diff-event", &event);
    let _ = app.emit(
        "file-diff-event",
        FileDiffPayload {
            path: path.to_string_lossy().to_string(),
            event,
            status,
        },
    );
}

fn emit_watch_error(app: &AppHandle, path: &Path, message: String) {
    let _ = app.emit(
        "file-watch-error",
        WatchErrorPayload {
            path: path.to_string_lossy().to_string(),
            message,
        },
    );
}

#[cfg(debug_assertions)]
fn debug_tauri_status(source: &str, status: &CoreStatus) {
    eprintln!(
        "[TAURI RESPONSE] source={} span={} active={} idle={} since_last_edit={} state={} document_id={} session_id={}",
        source,
        status.session_span_seconds,
        status.session_active_seconds,
        status.session_idle_seconds,
        status.since_last_edit_seconds,
        status.state,
        status.document_id.as_deref().unwrap_or("-"),
        status.session_id.as_deref().unwrap_or("-"),
    );
}

#[cfg(not(debug_assertions))]
fn debug_tauri_status(_source: &str, _status: &CoreStatus) {}

#[cfg(debug_assertions)]
fn debug_tauri_event(source: &str, event: &EditEvent) {
    eprintln!(
        "[TAURI EVENT] source={} event_id={} raw=+{} -{} writing=+{} -{}",
        source,
        event.transaction_id,
        event.inserted_chars,
        event.deleted_chars,
        event.writing_inserted_units.unwrap_or_default(),
        event.writing_deleted_units.unwrap_or_default(),
    );
}

#[cfg(not(debug_assertions))]
fn debug_tauri_event(_source: &str, _event: &EditEvent) {}

#[cfg(debug_assertions)]
fn debug_runtime_config(source: &str, config: &RuntimeTimingConfig) {
    eprintln!(
        "[CORE CONFIG] source={} mode={} timing_source={} idle_threshold={} session_end_threshold={}",
        source,
        config.mode,
        config.source,
        config.idle_threshold_seconds,
        config.session_end_threshold_seconds,
    );
}

#[cfg(not(debug_assertions))]
fn debug_runtime_config(_source: &str, _config: &RuntimeTimingConfig) {}

#[tauri::command]
fn get_watch_diagnostics(state: State<'_, AppState>) -> WatchDiagnostics {
    let Ok(watch) = state.watch.lock() else {
        return WatchDiagnostics {
            path: None,
            document_id: None,
            baseline_chars: None,
            current_file_chars: None,
            last_good_hash_prefix: None,
            watcher_state: "lock_error".to_string(),
            pending_atomic_replace: false,
        };
    };
    let path = watch.path.clone();
    let current_file_chars = path.as_deref().and_then(|path| {
        read_memory_snapshot(path)
            .ok()
            .map(|snapshot| snapshot.content.chars().count())
    });
    WatchDiagnostics {
        path: path.as_ref().map(|path| path.to_string_lossy().to_string()),
        document_id: watch.document_id.clone(),
        baseline_chars: watch
            .last_good_snapshot
            .as_ref()
            .map(|snapshot| snapshot.content.chars().count()),
        current_file_chars,
        last_good_hash_prefix: watch
            .last_good_hash
            .as_deref()
            .map(|hash| hash[..hash.len().min(12)].to_string()),
        watcher_state: if watch.path.is_none() {
            "stopped".to_string()
        } else if watch.pending_atomic_replace {
            "pending_atomic_replace".to_string()
        } else {
            "watching".to_string()
        },
        pending_atomic_replace: watch.pending_atomic_replace,
    }
}

fn snapshot_to_monitor(snapshot: &DocumentSnapshot) -> MonitorDocument {
    MonitorDocument {
        document_id: snapshot.document_id.clone(),
        path: snapshot.path.clone(),
        hash: snapshot.hash.clone(),
        mtime_ms: snapshot.mtime_ms,
        size: snapshot.size,
    }
}

fn start_tracking(
    path: String,
    mode: WatchMode,
    state: &AppState,
    app: &AppHandle,
) -> Result<(DocumentSnapshot, MemorySnapshot), String> {
    let _processing = state
        .file_processing
        .lock()
        .map_err(|_| "File processing lock is poisoned".to_string())?;
    reset_tracking_state(state)?;
    let snapshot = read_document(Path::new(&path))?;
    let memory = MemorySnapshot {
        content: snapshot.content.clone(),
        hash: snapshot.hash.clone(),
        mtime_ms: snapshot.mtime_ms,
        size: snapshot.size,
    };
    let mut core = state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?;
    let document_id = core
        .open_document(&snapshot)
        .map_err(|error| error.to_string())?;
    drop(core);
    let mut snapshot = snapshot;
    snapshot.document_id = document_id.clone();
    configure_watcher(
        app,
        Path::new(&snapshot.path),
        &document_id,
        mode,
        memory.clone(),
    )?;
    refresh_status_revision(state)?;
    Ok((snapshot, memory))
}

#[tauri::command]
fn start_monitor(
    path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<MonitorDocument, String> {
    let (snapshot, _) = start_tracking(path, WatchMode::FileDiff, state.inner(), &app)?;
    let document = snapshot_to_monitor(&snapshot);
    let _ = app.emit(
        "tracking-changed",
        TrackingChangedPayload {
            document: Some(document.clone()),
        },
    );
    Ok(document)
}

#[tauri::command]
fn stop_tracking(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    let _processing = state
        .file_processing
        .lock()
        .map_err(|_| "File processing lock is poisoned".to_string())?;
    reset_tracking_state(state.inner())?;
    refresh_status_revision(state.inner())?;
    let _ = app.emit(
        "tracking-changed",
        TrackingChangedPayload { document: None },
    );
    Ok(())
}

#[tauri::command]
fn open_document(
    path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<DocumentSnapshot, String> {
    let (snapshot, _) = start_tracking(path, WatchMode::EditorNotice, state.inner(), &app)?;
    Ok(snapshot)
}

#[tauri::command]
fn save_document(
    path: String,
    document_id: String,
    content: String,
    state: State<'_, AppState>,
) -> Result<SaveResult, String> {
    let path = canonical_markdown_path(Path::new(&path))?;
    let before_hash = current_hash(&path)?;
    let mut core = state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?;
    core.ensure_save_is_safe(&document_id, &path, &before_hash)
        .map_err(|error| error.to_string())?;
    atomic_save(&path, &content)?;
    let bytes = content.as_bytes();
    let metadata =
        fs::metadata(&path).map_err(|error| format!("Cannot stat saved file: {error}"))?;
    let result = SaveResult {
        path: path.to_string_lossy().to_string(),
        hash: file_hash(bytes),
        mtime_ms: mtime_ms(&metadata),
        size: metadata.len(),
    };
    core.mark_saved(&result.hash, result.mtime_ms, result.size, &content)
        .map_err(|error| error.to_string())?;
    drop(core);

    if let Ok(mut watch) = state.watch.lock() {
        watch.expected_hash = Some(result.hash.clone());
        if watch.path.as_deref() == Some(path.as_path()) {
            accept_last_good_snapshot(
                &mut watch,
                MemorySnapshot {
                    content,
                    hash: result.hash.clone(),
                    mtime_ms: result.mtime_ms,
                    size: result.size,
                },
            );
        }
    }
    Ok(result)
}

#[tauri::command]
fn record_edit(event: EditEvent, state: State<'_, AppState>) -> Result<CoreStatus, String> {
    debug_tauri_event("record_edit", &event);
    let mut core = state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?;
    let status = core.record_edit(event).map_err(|error| error.to_string())?;
    drop(core);
    let tracking = tracking_path(state.inner())?.is_some();
    sync_status_revision(state.inner(), &status, tracking)?;
    debug_tauri_status("record_edit", &status);
    Ok(status)
}

#[tauri::command]
fn get_writing_delta(old_markdown: String, new_markdown: String) -> WritingDelta {
    writing_delta(&old_markdown, &new_markdown)
}

#[tauri::command]
fn get_core_status(state: State<'_, AppState>) -> Result<CoreStatus, String> {
    let mut core = state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?;
    let status = core.status().map_err(|error| error.to_string())?;
    drop(core);
    let tracking = tracking_path(state.inner())?.is_some();
    sync_status_revision(state.inner(), &status, tracking)?;
    debug_tauri_status("get_core_status", &status);
    Ok(status)
}

#[tauri::command]
fn get_runtime_config(state: State<'_, AppState>) -> Result<RuntimeTimingConfig, String> {
    let core = state
        .core
        .lock()
        .map_err(|_| "Writing Core lock is poisoned".to_string())?;
    Ok(core.runtime_timing())
}

#[tauri::command]
fn apply_debug_timing(
    idle_threshold_seconds: i64,
    session_end_threshold_seconds: i64,
    state: State<'_, AppState>,
) -> Result<RuntimeTimingConfig, String> {
    #[cfg(not(debug_assertions))]
    {
        let _ = (idle_threshold_seconds, session_end_threshold_seconds, state);
        return Err("Debug timing overrides are unavailable in release builds".to_string());
    }

    #[cfg(debug_assertions)]
    {
        let mut core = state
            .core
            .lock()
            .map_err(|_| "Writing Core lock is poisoned".to_string())?;
        let config = core
            .apply_debug_timing(idle_threshold_seconds, session_end_threshold_seconds)
            .map_err(|error| error.to_string())?;
        debug_runtime_config("apply_debug_timing", &config);
        Ok(config)
    }
}

#[tauri::command]
fn get_window_capability() -> WindowCapability {
    detect_window_capability()
}

fn detect_window_capability() -> WindowCapability {
    let configured_backend = env::var("GDK_BACKEND")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if configured_backend
        .split(',')
        .any(|backend| backend.trim() == "x11")
    {
        let backend = if env::var_os("WAYLAND_DISPLAY").is_some() {
            "xwayland"
        } else {
            "x11"
        };
        return WindowCapability {
            backend: backend.to_string(),
            supported: true,
            reason: "GTK is configured to use the X11 backend; verify actual stacking manually."
                .to_string(),
        };
    }
    if env::var_os("WAYLAND_DISPLAY").is_some() {
        return WindowCapability {
            backend: "native_wayland".to_string(),
            supported: false,
            reason: "Native Wayland has no standard application-controlled always-on-top API."
                .to_string(),
        };
    }
    if env::var_os("DISPLAY").is_some() {
        return WindowCapability {
            backend: "x11".to_string(),
            supported: true,
            reason: "GTK is using an X11 display; verify actual stacking manually.".to_string(),
        };
    }
    WindowCapability {
        backend: "unknown".to_string(),
        supported: false,
        reason: "No supported display backend was detected.".to_string(),
    }
}

#[tauri::command]
fn get_window_state_diagnostics(
    app: AppHandle,
    state_filename: String,
) -> Result<WindowStateDiagnostics, String> {
    let filename_path = Path::new(&state_filename);
    if state_filename.is_empty()
        || filename_path.is_absolute()
        || filename_path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("Invalid window state filename".to_string());
    }

    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window is unavailable".to_string())?;
    let size = window
        .inner_size()
        .map_err(|error| format!("Cannot read current window size: {error}"))?;
    let position = window
        .outer_position()
        .map_err(|error| format!("Cannot read current window position: {error}"))?;
    let state_path = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("Cannot resolve app config directory: {error}"))?
        .join(&state_filename);

    let (stored_state_exists, stored_state, stored_state_parse_error) = match fs::read(&state_path)
    {
        Ok(bytes) => match serde_json::from_slice::<HashMap<String, StoredWindowState>>(&bytes) {
            Ok(states) => (true, states.get(window.label()).cloned(), None),
            Err(error) => (true, None, Some(error.to_string())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, None, None),
        Err(error) => {
            return Err(format!("Cannot read window state file: {error}"));
        }
    };

    Ok(WindowStateDiagnostics {
        backend: detect_window_capability().backend,
        label: window.label().to_string(),
        current_width: size.width,
        current_height: size.height,
        current_x: position.x,
        current_y: position.y,
        state_filename,
        state_path: state_path.to_string_lossy().to_string(),
        stored_state_exists,
        stored_state,
        stored_state_parse_error,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_window_state::Builder::default().build());

    builder
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)?;
            let db_path = data_dir.join("writing-core.sqlite");
            let core_config = CoreConfig::from_env();
            debug_runtime_config("startup", &core_config.runtime_timing());
            let core = WritingCore::new(db_path, core_config)
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)?;
            let service_instance_id = Uuid::new_v4().to_string();
            app.manage(AppState {
                core: Mutex::new(core),
                watcher: Mutex::new(None),
                watch: Mutex::new(WatchState::default()),
                file_processing: Mutex::new(()),
                service_instance_id,
                revision: Mutex::new(RevisionTracker::new()),
                status_api: Mutex::new(None),
            });
            let status_api = StatusApiRuntime::start(app.handle().clone());
            if let Ok(mut runtime) = app.state::<AppState>().status_api.lock() {
                *runtime = Some(status_api);
            } else {
                eprintln!("[STATUS API] runtime registration failed; API will stop with the app");
            }
            #[cfg(desktop)]
            {
                use tauri_plugin_window_state::AppHandleExt;

                let filename = app.handle().filename();
                let state_path = app
                    .path()
                    .app_config_dir()
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)?
                    .join(&filename);
                eprintln!(
                    "[WINDOW STATE] plugin initialized; automatic restore is attached to window-ready"
                );
                eprintln!(
                    "[WINDOW STATE] state file: {} (exists={})",
                    state_path.display(),
                    state_path.is_file()
                );
            }
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            start_monitor,
            stop_tracking,
            open_document,
            save_document,
            record_edit,
            get_writing_delta,
            get_core_status,
            get_runtime_config,
            apply_debug_timing,
            get_watch_diagnostics,
            get_window_capability,
            get_window_state_diagnostics
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            RunEvent::Ready => {
                if let Some(window) = app.get_webview_window("main") {
                    match (window.inner_size(), window.outer_position()) {
                        (Ok(size), Ok(position)) => eprintln!(
                            "[WINDOW STATE] restore result label={} size={}x{} position=({}, {})",
                            window.label(),
                            size.width,
                            size.height,
                            position.x,
                            position.y
                        ),
                        (size, position) => eprintln!(
                            "[WINDOW STATE] restore result read failed label={} size={:?} position={:?}",
                            window.label(),
                            size.err().map(|error| error.to_string()),
                            position.err().map(|error| error.to_string())
                        ),
                    }
                }
            }
            RunEvent::Exit => shutdown_status_api(app),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind};

    fn status_for_api_test() -> CoreStatus {
        CoreStatus {
            document_id: Some("doc-1".to_string()),
            session_id: Some("session-1".to_string()),
            session_started_at: Some(0),
            state: "active".to_string(),
            since_last_edit_seconds: 8,
            session_span_seconds: 4821,
            session_active_seconds: 3510,
            session_idle_seconds: 1311,
            session_inserted_chars: 10,
            session_deleted_chars: 2,
            session_net_chars: 8,
            session_writing_inserted_units: Some(12),
            session_writing_deleted_units: Some(3),
            session_writing_net_units: Some(9),
            session_writing_count_version: Some(1),
            document_writing_count: Some(8478),
            tracking_origin: Some("file_diff".to_string()),
        }
    }

    fn waiting_api_status() -> CoreStatus {
        CoreStatus {
            document_id: None,
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
            document_writing_count: None,
            tracking_origin: None,
        }
    }

    fn memory_snapshot(content: &str) -> MemorySnapshot {
        MemorySnapshot {
            content: content.to_string(),
            hash: file_hash(content.as_bytes()),
            mtime_ms: 0,
            size: content.len() as u64,
        }
    }

    #[test]
    fn atomic_replace_append_compares_against_last_good_snapshot() {
        let old_text = "a".repeat(8628);
        let new_text = format!("{old_text}增加七字符🙂!");
        let mut last_good = memory_snapshot(&old_text);
        let old_hash = last_good.hash.clone();
        let pending_atomic_replace = true;
        assert_eq!(last_good.content.chars().count(), 8628);
        assert_eq!(last_good.hash, old_hash);
        assert!(pending_atomic_replace);

        let next = memory_snapshot(&new_text);
        let summary = file_diff_summary(&last_good, &next).expect("one real diff");
        assert_eq!(summary.inserted_chars, 7);
        assert_eq!(summary.deleted_chars, 0);

        last_good = next;
        let pending_atomic_replace = false;
        assert!(!pending_atomic_replace);
        assert_eq!(last_good.content.chars().count(), 8635);
        assert_eq!(last_good.hash, file_hash(new_text.as_bytes()));
    }

    #[test]
    fn atomic_replace_replace_text_reports_inserted_and_deleted_scalars() {
        let old_text = "欧林索斯走进花园。";
        let new_text = "欧林索斯牵着林祺走进夜花庭。";
        let previous = memory_snapshot(old_text);
        let next = memory_snapshot(new_text);

        let summary = file_diff_summary(&previous, &next).expect("one real diff");
        assert_eq!(summary.inserted_chars, 6);
        assert_eq!(summary.deleted_chars, 1);
    }

    #[test]
    fn temporary_missing_target_retains_last_good_baseline() {
        let previous = memory_snapshot(&"a".repeat(8628));
        let before = previous.clone();
        let mut watch = WatchState {
            last_good_hash: Some(previous.hash.clone()),
            last_good_snapshot: Some(previous),
            ..WatchState::default()
        };

        mark_target_temporarily_missing(&mut watch);

        assert_eq!(
            watch.last_good_snapshot.as_ref().unwrap().content,
            before.content
        );
        assert_eq!(watch.last_good_hash.as_deref(), Some(before.hash.as_str()));
        assert!(watch.pending_atomic_replace);
    }

    #[test]
    fn duplicate_notify_with_same_hash_produces_no_file_diff() {
        let previous = memory_snapshot("相同正文");
        let duplicate = memory_snapshot("相同正文");

        assert_eq!(file_diff_summary(&previous, &duplicate), None);
        assert_eq!(file_diff_summary(&previous, &duplicate), None);
    }

    #[test]
    fn live_snapshot_becomes_the_shared_baseline_and_saved_hash_is_deduped() {
        let disk = memory_snapshot(&"旧".repeat(1_000));
        let live = memory_snapshot(&"旧".repeat(1_200));
        let decision = live_snapshot_decision(&disk, None, 1, &live).expect("live decision");
        let LiveSnapshotDecision::Changed(summary) = decision else {
            panic!("live snapshot should be accepted as a change");
        };
        assert_eq!(summary.inserted_chars, 200);

        let mut watch = WatchState {
            last_good_hash: Some(disk.hash.clone()),
            last_good_snapshot: Some(disk),
            ..WatchState::default()
        };
        watch.expected_hash = Some(live.hash.clone());
        accept_last_good_snapshot(&mut watch, live.clone());

        assert_eq!(
            file_diff_summary(watch.last_good_snapshot.as_ref().unwrap(), &live),
            None,
            "saving the already accepted live snapshot must not create a second diff"
        );
    }

    #[test]
    fn editor_revisions_are_deduped_per_editor_instance() {
        let previous = memory_snapshot("正文");
        let next = memory_snapshot("正文一");
        assert!(matches!(
            live_snapshot_decision(&previous, Some(7), 7, &next).unwrap(),
            LiveSnapshotDecision::StaleRevision
        ));
        assert!(matches!(
            live_snapshot_decision(&previous, None, 1, &next).unwrap(),
            LiveSnapshotDecision::Changed(_)
        ));
    }

    #[test]
    fn access_events_from_snapshot_reads_are_not_file_changes() {
        let access = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)));
        let create = Event::new(EventKind::Create(CreateKind::File));

        assert!(!is_processable_file_event(&access));
        assert!(is_processable_file_event(&create));
    }

    #[test]
    fn status_api_no_tracking_uses_null_document_and_session() {
        let response = status_api_response("core-instance", 1, false, None, &waiting_api_status());
        let json = serde_json::to_value(response).expect("status JSON");

        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["service_instance_id"], "core-instance");
        assert_eq!(json["revision"], 1);
        assert_eq!(json["tracking"], false);
        assert_eq!(json["activity_state"], "inactive");
        assert!(json["document"].is_null());
        assert!(json["session"].is_null());
    }

    #[test]
    fn status_api_tracking_without_session_keeps_document_and_null_session() {
        let mut status = waiting_api_status();
        status.document_id = Some("doc-1".to_string());
        status.document_writing_count = Some(8471);
        let response = status_api_response(
            "core-instance",
            2,
            true,
            Some(Path::new("/private/mvp-smoke.md")),
            &status,
        );
        let json = serde_json::to_value(response).expect("status JSON");

        assert_eq!(json["activity_state"], "inactive");
        assert_eq!(json["document"]["id"], "doc-1");
        assert_eq!(json["document"]["name"], "mvp-smoke.md");
        assert_eq!(
            json["document"]["path_hash"],
            path_identity_hash(Path::new("/private/mvp-smoke.md"))
        );
        assert_eq!(json["document"]["writing_count"], 8471);
        assert!(json["session"].is_null());
    }

    #[test]
    fn status_api_active_session_serializes_writing_facts_without_raw_or_path() {
        let response = status_api_response(
            "core-instance",
            3,
            true,
            Some(Path::new("/private/mvp-smoke.md")),
            &status_for_api_test(),
        );
        let json = serde_json::to_value(response).expect("status JSON");

        assert_eq!(json["activity_state"], "active");
        assert_eq!(json["session"]["id"], "session-1");
        assert_eq!(json["session"]["started_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["session"]["writing_inserted_units"], 12);
        assert_eq!(json["session"]["writing_deleted_units"], 3);
        assert_eq!(json["session"]["writing_net_units"], 9);
        assert!(json.get("content").is_none());
        assert!(json.get("path").is_none());
        assert!(json["session"].get("session_inserted_chars").is_none());
    }

    #[test]
    fn status_revision_ignores_repeated_reads_and_wall_clock_fields() {
        let mut tracker = RevisionTracker::new();
        let inactive = waiting_api_status();
        assert_eq!(
            tracker.observe(StatusFingerprint::from_status(&inactive, false)),
            1
        );

        let mut tracked = waiting_api_status();
        tracked.document_id = Some("doc-1".to_string());
        tracked.document_writing_count = Some(10);
        assert_eq!(
            tracker.observe(StatusFingerprint::from_status(&tracked, true)),
            2
        );
        assert_eq!(
            tracker.observe(StatusFingerprint::from_status(&tracked, true)),
            2
        );

        let active = status_for_api_test();
        assert_eq!(
            tracker.observe(StatusFingerprint::from_status(&active, true)),
            3
        );

        let mut same_facts_later = active.clone();
        same_facts_later.session_span_seconds += 1;
        same_facts_later.session_active_seconds += 1;
        same_facts_later.since_last_edit_seconds += 1;
        assert_eq!(
            tracker.observe(StatusFingerprint::from_status(&same_facts_later, true)),
            3
        );

        same_facts_later.session_net_chars += 1;
        assert_eq!(
            tracker.observe(StatusFingerprint::from_status(&same_facts_later, true)),
            4
        );
    }
}
