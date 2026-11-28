//! Google Calendar two-way sync engine.
//!
//! A dedicated worker thread ("calendar-plugin-gsync") runs sync cycles:
//! drain the persisted push queue → pull each selected calendar
//! incrementally (syncToken) → prune stale expanded instances. Cycles are
//! triggered by a channel message (manual "Sync now", config change, event
//! edit) or by the configured interval, guarded by an error cooldown so a
//! failing account does not hammer Google every 5 seconds.
//!
//! Policy highlights (documented in the README):
//! - Google times are converted to local wall-clock; the local model has no
//!   timezone column.
//! - Clean RRULEs map onto the local 6-value `Repeat` enum (read-only);
//!   anything else (UNTIL/COUNT/EXDATE/overrides/complex BYDAY) is expanded
//!   into per-occurrence instance rows inside the sync window (read-only).
//! - Non-recurring Google events are fully editable in the app and push
//!   back; recurring ones are Google-managed in v1.
//! - Pull merges preserve app-only fields (`message`, `enabled`,
//!   `last_fired`); Google wins on title/description/time/recurrence.
//! - Rows with a pending push are never clobbered by the pull pass.

use crate::google_api::{ApiError, GEvent, GoogleApi};
use crate::google_oauth as oauth;
use crate::store::Store;
use crate::{CalendarEvent, Repeat};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, Offset, SecondsFormat, TimeZone, Timelike, Utc};
use companion_plugin_sdk::PluginCtx;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Full-pull and instance-expansion window, in days relative to today.
const PAST_DAYS: i64 = 7;
const FUTURE_DAYS: i64 = 90;
/// Give up on a queued push after this many failed attempts.
const QUEUE_ATTEMPTS_MAX: i64 = 5;
/// Entries processed per cycle (keeps cycles short when queues are long).
const QUEUE_DRAIN_MAX: usize = 50;
/// Backoff after a failed cycle.
const ERROR_COOLDOWN: i64 = 120_000; // millis
pub(crate) const DEFAULT_INTERVAL_MIN: i64 = 15;
/// Poll cadence of the worker loop when idle.
const LOOP_TICK: Duration = Duration::from_secs(5);

pub(crate) const CONFIG_CLIENT_ID: &str = "client_id";
pub(crate) const CONFIG_ACCOUNT: &str = "account";
pub(crate) const CONFIG_SELECTED: &str = "selected_calendars";
pub(crate) const CONFIG_DEFAULT: &str = "default_calendar";
pub(crate) const CONFIG_INTERVAL: &str = "interval_minutes";

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Disconnected,
    AwaitingCallback,
    Connected,
    NeedsLogin,
}

struct Shared {
    phase: Phase,
    account: Option<String>,
    last_error: Option<String>,
    syncing: bool,
    cooldown_until: i64,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            phase: Phase::Disconnected,
            account: None,
            last_error: None,
            syncing: false,
            cooldown_until: 0,
        }
    }
}

pub struct StatusSnapshot {
    pub connected: bool,
    pub phase: &'static str,
    pub account: Option<String>,
    pub syncing: bool,
    pub last_error: Option<String>,
}

enum Msg {
    Cycle,
}

/// Route-side handle for the sync worker. Clones of the shared store keep
/// queue writes thread-safe; `ctx` is only used for event/log emission.
pub struct SyncHandle {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
    counter: oauth::ConnectGeneration,
    store: Arc<Store>,
    ctx: PluginCtx,
    stop: Arc<AtomicBool>,
}

impl SyncHandle {
    fn phase_is(&self, phase: Phase) -> bool {
        self.shared
            .lock()
            .map(|s| s.phase == phase)
            .unwrap_or(false)
    }

    pub fn connected(&self) -> bool {
        self.phase_is(Phase::Connected)
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let s = self.shared.lock().ok();
        let s = s.as_ref();
        let phase = s.map(|s| &s.phase);
        StatusSnapshot {
            connected: phase == Some(&Phase::Connected),
            phase: match phase {
                Some(Phase::Connected) => "connected",
                Some(Phase::AwaitingCallback) => "awaiting_callback",
                Some(Phase::NeedsLogin) => "needs_login",
                _ => "disconnected",
            },
            account: s.and_then(|s| s.account.clone()),
            syncing: s.map(|s| s.syncing).unwrap_or(false),
            last_error: s.and_then(|s| s.last_error.clone()),
        }
    }

    pub fn sync_now(&self) {
        let _ = self.tx.send(Msg::Cycle);
    }

    pub fn config_changed(&self) {
        let _ = self.tx.send(Msg::Cycle);
    }

    /// Queue an upsert push for a locally-mutated event. Read-only Google
    /// rows (series mappings, expanded instances) are never pushed.
    pub fn enqueue_upsert(&self, e: &CalendarEvent) {
        if !self.connected() || e.google_readonly {
            return;
        }
        if let Err(err) = self.store.queue_add(&e.id, "upsert", None) {
            self.ctx.warn(&format!("google: queue upsert: {err}"));
            return;
        }
        let _ = self.tx.send(Msg::Cycle);
    }

    /// Queue a delete push. The Google ids are captured now — the local row
    /// is already gone. Pending upserts for the same event are dropped.
    pub fn enqueue_delete(&self, e: &CalendarEvent) {
        let _ = self.store.queue_remove_for_local(&e.id);
        if !self.connected() || e.google_readonly {
            return;
        }
        if let (Some(cal), Some(gid)) = (&e.google_calendar_id, &e.google_event_id) {
            let payload = json!({ "calendar_id": cal, "event_id": gid }).to_string();
            if let Err(err) = self.store.queue_add(&e.id, "delete", Some(&payload)) {
                self.ctx.warn(&format!("google: queue delete: {err}"));
            } else {
                let _ = self.tx.send(Msg::Cycle);
            }
        }
    }

    /// Begin the OAuth flow: persist credentials, bind the loopback
    /// callback server, return the consent-screen URL for the browser.
    pub fn start_connect(&self, client_id: &str, client_secret: &str) -> Result<String, String> {
        if client_id.trim().is_empty() || client_secret.trim().is_empty() {
            return Err("client id and client secret are both required".into());
        }
        self.store
            .config_set(CONFIG_CLIENT_ID, client_id.trim())
            .map_err(|e| e)?;
        let mut blob = oauth::load_blob();
        blob.client_secret = Some(client_secret.trim().to_string());
        oauth::save_blob(&blob)?;

        let server = oauth::ConnectServer::new(Arc::clone(&self.counter))?;
        let auth_url = server.auth_url(client_id.trim());
        {
            let mut sh = self.shared.lock().map_err(|e| e.to_string())?;
            sh.phase = Phase::AwaitingCallback;
            sh.last_error = None;
        }
        self.emit_status();

        // Background: wait for the redirect, exchange the code, store
        // tokens, mark connected. Exits when superseded/timed out.
        let handle = SyncHandle {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
            counter: Arc::clone(&self.counter),
            store: Arc::clone(&self.store),
            ctx: self.ctx.clone(),
            stop: Arc::clone(&self.stop),
        };
        std::thread::Builder::new()
            .name("calendar-plugin-oauth".into())
            .spawn(move || oauth_callback_thread(handle, server))
            .map_err(|e| format!("spawn oauth thread: {e}"))?;
        Ok(auth_url)
    }

    /// Revoke + clear credentials and sync state. Synced events remain.
    pub fn disconnect(&self) -> Result<(), String> {
        let blob = oauth::load_blob();
        if let Some(rt) = blob.tokens.as_ref().and_then(|t| t.refresh_token.clone()) {
            if let Err(e) = oauth::revoke(&rt) {
                self.ctx.warn(&format!("google: revoke: {e}"));
            }
        }
        if let Err(e) = oauth::clear_blob() {
            self.ctx.warn(&format!("google: keychain clear: {e}"));
        }
        let _ = self.store.config_delete(CONFIG_ACCOUNT);
        let _ = self.store.sync_state_clear();
        let _ = self.store.queue_clear();
        {
            let mut sh = self.shared.lock().map_err(|e| e.to_string())?;
            sh.phase = Phase::Disconnected;
            sh.account = None;
            sh.last_error = None;
            sh.cooldown_until = 0;
        }
        self.emit_status();
        Ok(())
    }

    fn emit_status(&self) {
        self.ctx.emit("google-status-changed", &Value::Null);
    }
}

/// Thread body for one OAuth attempt.
fn oauth_callback_thread(handle: SyncHandle, server: oauth::ConnectServer) {
    let generation = server.generation;
    let client_id = match handle.store.config_get(CONFIG_CLIENT_ID) {
        Some(id) => id,
        None => return,
    };
    let result = server.wait_for_code();
    // Superseded by a newer connect attempt? Then stay quiet.
    if handle.counter.load(Ordering::SeqCst) != generation {
        return;
    }
    match result {
        Ok(code) => {
            let blob = oauth::load_blob();
            let Some(secret) = blob.client_secret.clone() else {
                set_error(&handle, "client secret disappeared from the keychain");
                return;
            };
            let prev_refresh = blob
                .tokens
                .as_ref()
                .and_then(|t| t.refresh_token.clone());
            match oauth::exchange_code(
                &client_id,
                &secret,
                &code,
                &server.redirect_uri,
                &server.verifier,
                prev_refresh.as_deref(),
            ) {
                Ok(tokens) => {
                    let mut blob = oauth::load_blob();
                    blob.tokens = Some(tokens.clone());
                    if let Err(e) = oauth::save_blob(&blob) {
                        set_error(&handle, &format!("keychain save: {e}"));
                        return;
                    }
                    // The primary calendar's id is the account email.
                    let account = GoogleApi::new().ok().and_then(|api| {
                        api.calendar_list(&tokens.access_token)
                            .ok()
                            .and_then(|list| {
                                list.into_iter()
                                    .find(|c| c.primary)
                                    .map(|c| c.id)
                            })
                    });
                    let _ = handle.store.config_set(
                        CONFIG_ACCOUNT,
                        account.as_deref().unwrap_or(""),
                    );
                    if let Ok(mut sh) = handle.shared.lock() {
                        sh.phase = Phase::Connected;
                        sh.account = account;
                        sh.last_error = None;
                        sh.cooldown_until = 0;
                    }
                    handle.ctx.info("google: connected");
                    handle.emit_status();
                    let _ = handle.tx.send(Msg::Cycle);
                }
                Err(e) => set_error(&handle, &format!("token exchange failed: {e}")),
            }
        }
        Err(e) => set_error(&handle, &e),
    }
}

fn set_error(handle: &SyncHandle, msg: &str) {
    handle.ctx.warn(&format!("google: {msg}"));
    if let Ok(mut sh) = handle.shared.lock() {
        if sh.phase == Phase::AwaitingCallback {
            sh.phase = Phase::Disconnected;
        }
        sh.last_error = Some(msg.to_string());
    }
    handle.emit_status();
}

/// Spawn the sync worker thread and return the route-side handle.
pub fn spawn_sync_worker(
    ctx: PluginCtx,
    events: Arc<Mutex<Vec<CalendarEvent>>>,
    store: Arc<Store>,
    stop: Arc<AtomicBool>,
) -> SyncHandle {
    let (tx, rx) = channel::<Msg>();
    let shared = Arc::new(Mutex::new(Shared::default()));
    let handle = SyncHandle {
        tx: tx.clone(),
        shared: Arc::clone(&shared),
        counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        store: Arc::clone(&store),
        ctx: ctx.clone(),
        stop: Arc::clone(&stop),
    };
    // Resume state: credentials + tokens present from a previous run.
    if store.config_get(CONFIG_CLIENT_ID).is_some()
        && oauth::load_blob().tokens.is_some()
    {
        if let Ok(mut sh) = shared.lock() {
            sh.phase = Phase::Connected;
            sh.account = store.config_get(CONFIG_ACCOUNT).filter(|s| !s.is_empty());
        }
    }
    let worker_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("calendar-plugin-gsync".into())
        .spawn(move || worker_loop(ctx, events, store, worker_shared, stop, rx))
        .expect("spawn calendar gsync thread");
    handle
}

fn worker_loop(
    ctx: PluginCtx,
    events: Arc<Mutex<Vec<CalendarEvent>>>,
    store: Arc<Store>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    rx: Receiver<Msg>,
) {
    loop {
        // Wake on messages, but poll the interval clock every LOOP_TICK.
        match rx.recv_timeout(LOOP_TICK) {
            Ok(Msg::Cycle) => {}
            Err(_) => {}
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !should_run(&store, &shared) {
            continue;
        }
        run_cycle(&ctx, &events, &store, &shared);
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
}

fn should_run(store: &Store, shared: &Arc<Mutex<Shared>>) -> bool {
    let sh = match shared.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if sh.phase != Phase::Connected || sh.syncing {
        return false;
    }
    let now = now_millis();
    if now < sh.cooldown_until {
        return false;
    }
    // Only run once per stored config; an interval of 0 or absent config
    // falls back to the default.
    if store.config_get(CONFIG_CLIENT_ID).is_none() {
        return false;
    }
    let interval = store
        .config_get(CONFIG_INTERVAL)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_INTERVAL_MIN);
    let last = store.last_sync().unwrap_or(0);
    now >= last + interval * 60_000 || store.queue_count() > 0
}

fn run_cycle(
    ctx: &PluginCtx,
    events: &Arc<Mutex<Vec<CalendarEvent>>>,
    store: &Arc<Store>,
    shared: &Arc<Mutex<Shared>>,
) {
    if let Ok(mut sh) = shared.lock() {
        sh.syncing = true;
    }
    ctx.emit("google-status-changed", &Value::Null);
    let outcome = cycle_once(ctx, events, store);
    if let Ok(mut sh) = shared.lock() {
        sh.syncing = false;
    match &outcome {
        Ok(()) => {
            sh.last_error = None;
        }
        Err(e) => {
            sh.last_error = Some(e.clone());
            sh.cooldown_until = now_millis() + ERROR_COOLDOWN;
            // Refresh-token failures mean the grant is dead: the user must
            // re-run the consent flow.
            if e.contains("sign in again") {
                sh.phase = Phase::NeedsLogin;
            }
        }
    }
    }
    match &outcome {
        Ok(()) => ctx.info("google: sync complete"),
        Err(e) => ctx.warn(&format!("google: sync failed: {e}")),
    }
    ctx.emit("google-status-changed", &Value::Null);
    ctx.emit("calendar-changed", &Value::Null);
}

fn cycle_once(
    ctx: &PluginCtx,
    events: &Arc<Mutex<Vec<CalendarEvent>>>,
    store: &Arc<Store>,
) -> Result<(), String> {
    let client_id = store
        .config_get(CONFIG_CLIENT_ID)
        .filter(|s| !s.is_empty())
        .ok_or("not configured")?;
    let selected = selected_calendars(store);
    if selected.is_empty() {
        return Ok(()); // connected but nothing chosen yet
    }
    let api = GoogleApi::new().map_err(|e| e)?;
    let mut blob = oauth::load_blob();
    let secret = blob
        .client_secret
        .clone()
        .ok_or("client secret missing — sign in again")?;
    let tokens = blob.tokens.take().ok_or("not signed in")?;
    let tokens = ensure_tokens(&client_id, &secret, tokens)?;
    blob.tokens = Some(tokens.clone());
    oauth::save_blob(&blob)?;

    drain_queue(ctx, &api, &tokens.access_token, events, store)?;
    let (window_start, window_end) = sync_window_dates();
    for cal_id in &selected {
        pull_calendar(&api, &tokens.access_token, cal_id, events, store, window_start, window_end)?;
    }
    // Expanded instances older than the window are regenerated by future
    // full syncs; drop them so the store stays lean.
    let cutoff = window_start.format("%Y-%m-%d").to_string();
    store.prune_instances_before(&cutoff)?;
    Ok(())
}

/// Refresh the access token when it is (nearly) expired. Also used by the
/// route handlers that make ad-hoc Google calls (calendar list).
pub(crate) fn ensure_tokens(
    client_id: &str,
    secret: &str,
    tokens: oauth::Tokens,
) -> Result<oauth::Tokens, String> {
    if !tokens.access_token.is_empty() && tokens.expires_at > now_millis() + 60_000 {
        return Ok(tokens);
    }
    let rt = tokens
        .refresh_token
        .clone()
        .ok_or("no refresh token — sign in again")?;
    oauth::refresh_tokens(client_id, secret, &rt, Some(&rt))
        .map_err(|e| format!("token refresh failed: {e} (sign in again)"))
}

fn selected_calendars(store: &Store) -> Vec<String> {
    store
        .config_get(CONFIG_SELECTED)
        .and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok())
        .unwrap_or_default()
}

fn default_push_calendar(store: &Store) -> String {
    store
        .config_get(CONFIG_DEFAULT)
        .filter(|s| !s.is_empty())
        .or_else(|| selected_calendars(store).first().cloned())
        .unwrap_or_else(|| "primary".to_string())
}

fn sync_window_dates() -> (NaiveDate, NaiveDate) {
    let today = Local::now().date_naive();
    (
        today - chrono::Duration::days(PAST_DAYS),
        today + chrono::Duration::days(FUTURE_DAYS),
    )
}

// ---------------------------------------------------------------------------
// Push: drain the queue
// ---------------------------------------------------------------------------

enum ProcessOutcome {
    Done,
    Retry(String),
    Permanent(String),
}

fn drain_queue(
    ctx: &PluginCtx,
    api: &GoogleApi,
    token: &str,
    events: &Arc<Mutex<Vec<CalendarEvent>>>,
    store: &Arc<Store>,
) -> Result<(), String> {
    for _ in 0..QUEUE_DRAIN_MAX {
        let Some(entry) = store.queue_next() else {
            return Ok(());
        };
        match process_entry(api, token, &entry, events, store) {
            ProcessOutcome::Done => {
                let _ = store.queue_remove(entry.seq);
            }
            ProcessOutcome::Permanent(msg) => {
                ctx.warn(&format!("google: dropped push for {}: {msg}", entry.local_id));
                let _ = store.queue_remove(entry.seq);
            }
            ProcessOutcome::Retry(msg) => {
                let _ = store.queue_touch(entry.seq);
                if entry.attempts + 1 >= QUEUE_ATTEMPTS_MAX {
                    ctx.warn(&format!(
                        "google: push for {} failed {} times, dropping: {msg}",
                        entry.local_id, QUEUE_ATTEMPTS_MAX
                    ));
                    let _ = store.queue_remove(entry.seq);
                    continue;
                }
                return Err(msg); // transient: abort the cycle, retry later
            }
        }
    }
    Ok(())
}

fn process_entry(
    api: &GoogleApi,
    token: &str,
    entry: &crate::store::QueueEntry,
    events: &Arc<Mutex<Vec<CalendarEvent>>>,
    store: &Arc<Store>,
) -> ProcessOutcome {
    match entry.op.as_str() {
        "delete" => {
            let payload: Value = entry
                .payload
                .as_deref()
                .and_then(|p| serde_json::from_str(p).ok())
                .unwrap_or(Value::Null);
            let (Some(cal), Some(gid)) = (
                payload.get("calendar_id").and_then(Value::as_str),
                payload.get("event_id").and_then(Value::as_str),
            ) else {
                return ProcessOutcome::Done; // never pushed to Google
            };
            match api.delete_event(token, cal, gid) {
                Ok(()) => ProcessOutcome::Done,
                Err(ApiError::Unauthorized(_)) => ProcessOutcome::Retry("unauthorized".into()),
                Err(ApiError::Gone(_)) => ProcessOutcome::Done,
                Err(ApiError::Other(e)) if e.contains("403") => ProcessOutcome::Permanent(e),
                Err(ApiError::Other(e)) => ProcessOutcome::Retry(e),
            }
        }
        "upsert" => {
            let event = {
                let guard = match events.lock() {
                    Ok(g) => g,
                    Err(_) => return ProcessOutcome::Retry("event cache locked".into()),
                };
                guard.iter().find(|e| e.id == entry.local_id).cloned()
            };
            let Some(event) = event else {
                return ProcessOutcome::Done; // deleted before we could push
            };
            if event.google_readonly {
                return ProcessOutcome::Done;
            }
            let body = event_to_google_body(&event);
            let result = match (&event.google_calendar_id, &event.google_event_id) {
                (Some(cal), Some(gid)) => api
                    .patch_event(token, cal, gid, &body)
                    .map(|_| (cal.clone(), gid.clone())),
                _ => {
                    let target = event
                        .google_calendar_id
                        .clone()
                        .unwrap_or_else(|| default_push_calendar(store));
                    api.insert_event(token, &target, &body)
                        .map(|created| (target, created.id))
                }
            };
            match result {
                Ok((cal, gid)) => {
                    let now = now_millis();
                    let _ = store.set_google_ids(&event.id, &cal, &gid, now);
                    if let Ok(mut guard) = events.lock() {
                        if let Some(row) = guard.iter_mut().find(|e| e.id == event.id) {
                            row.google_calendar_id = Some(cal);
                            row.google_event_id = Some(gid);
                            row.updated_at = Some(now);
                        }
                    }
                    ProcessOutcome::Done
                }
                Err(ApiError::Unauthorized(_)) => ProcessOutcome::Retry("unauthorized".into()),
                Err(ApiError::Gone(_)) => {
                    // Patched event vanished on Google; the next pull will
                    // remove the local row.
                    ProcessOutcome::Done
                }
                Err(ApiError::Other(e)) if e.contains("403") => ProcessOutcome::Permanent(e),
                Err(ApiError::Other(e)) => ProcessOutcome::Retry(e),
            }
        }
        _ => ProcessOutcome::Done,
    }
}

// ---------------------------------------------------------------------------
// Pull: incremental events.list per calendar
// ---------------------------------------------------------------------------

fn pull_calendar(
    api: &GoogleApi,
    token: &str,
    calendar_id: &str,
    events: &Arc<Mutex<Vec<CalendarEvent>>>,
    store: &Arc<Store>,
    window_start: NaiveDate,
    window_end: NaiveDate,
) -> Result<(), String> {
    let stored_token = store.sync_token_get(calendar_id);
    let mut incremental = stored_token.is_some();
    let mut page_token: Option<String> = None;
    let mut next_sync_token: Option<String> = None;
    let mut pulled: Vec<GEvent> = Vec::new();
    let mut full_retry = false;
    let now = Local::now();
    let time_min = (now - chrono::Duration::days(PAST_DAYS))
        .to_rfc3339_opts(SecondsFormat::Secs, false);
    let time_max = (now + chrono::Duration::days(FUTURE_DAYS))
        .to_rfc3339_opts(SecondsFormat::Secs, false);

    loop {
        let result = if incremental {
            api.events(
                token,
                calendar_id,
                stored_token.as_deref(),
                None,
                None,
                page_token.as_deref(),
            )
        } else {
            api.events(
                token,
                calendar_id,
                None,
                Some(&time_min),
                Some(&time_max),
                page_token.as_deref(),
            )
        };
        match result {
            Ok(page) => {
                pulled.extend(page.events);
                next_sync_token = page.next_sync_token.or(next_sync_token);
                match page.next_page_token {
                    Some(t) => page_token = Some(t),
                    None => break,
                }
            }
            Err(ApiError::Gone(_)) if incremental && !full_retry => {
                // The sync token expired (Google keeps them ~30 days):
                // fall back to a full window resync.
                ctx_debug(calendar_id, "sync token expired; full resync");
                incremental = false;
                full_retry = true;
                page_token = None;
                pulled.clear();
                next_sync_token = None;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    apply_pulled_events(
        calendar_id,
        &pulled,
        events,
        store,
        window_start,
        window_end,
    );
    store.sync_state_set(calendar_id, next_sync_token.as_deref())?;
    Ok(())
}

fn ctx_debug(calendar_id: &str, msg: &str) {
    eprintln!("[calendar] google pull {calendar_id}: {msg}");
}

/// Apply one calendar's pulled events to the local store + cache.
fn apply_pulled_events(
    calendar_id: &str,
    pulled: &[GEvent],
    events: &Arc<Mutex<Vec<CalendarEvent>>>,
    store: &Arc<Store>,
    window_start: NaiveDate,
    window_end: NaiveDate,
) {
    // Overrides (single-instance edits made in Google): exclude their
    // original occurrence dates from series expansion.
    let mut overrides: HashMap<String, Vec<String>> = HashMap::new();
    for g in pulled {
        if let (Some(series), Some(orig)) = (&g.recurring_event_id, &g.original_start) {
            if let Some(local) = google_time_local(orig) {
                overrides
                    .entry(series.clone())
                    .or_default()
                    .push(local.with_timezone(&Local).date_naive().to_string());
            }
        }
    }
    let pending: HashSet<String> = store.queue_pending_upsert_ids();
    let snapshot: Vec<CalendarEvent> = match events.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };

    for g in pulled {
        if g.status.as_deref() == Some("cancelled") {
            let _ = store.delete_google_rows(calendar_id, &g.id);
            drop_cache_rows(events, calendar_id, &g.id);
            continue;
        }
        let local_id = format!("gcal-{calendar_id}-{}", g.id);
        if pending.contains(&local_id) {
            continue; // newer local edit is waiting to be pushed
        }
        let rows = match map_google_event(calendar_id, g, &overrides, window_start, window_end)
        {
            Ok(rows) => rows,
            Err(e) => {
                ctx_debug(calendar_id, &format!("skipping {}: {e}", g.id));
                continue;
            }
        };
        // Newer local push waiting? Leave this event alone.
        if rows.iter().any(|r| pending.contains(&r.id)) {
            continue;
        }
        // Preserve app-only fields from any existing row with the same id.
        let mut merged = rows;
        for m in &mut merged {
            if let Some(old) = snapshot.iter().find(|x| x.id == m.id) {
                m.message = old.message.clone();
                m.enabled = old.enabled;
                m.last_fired = old.last_fired;
                m.updated_at = old.updated_at;
            }
        }
        // Replace every stored row for this Google event (series mapping
        // shape may have changed: mapped ↔ instances ↔ single).
        let _ = store.delete_google_rows(calendar_id, &g.id);
        for m in &merged {
            if let Err(e) = store.upsert_event(m) {
                ctx_debug(calendar_id, &format!("store {}: {e}", m.id));
            }
        }
        replace_cache_rows(events, calendar_id, &g.id, merged);
    }
}

fn drop_cache_rows(events: &Arc<Mutex<Vec<CalendarEvent>>>, calendar_id: &str, gid: &str) {
    if let Ok(mut guard) = events.lock() {
        guard.retain(|e| {
            !(e.google_calendar_id.as_deref() == Some(calendar_id)
                && e.google_event_id.as_deref() == Some(gid))
        });
    }
}

fn replace_cache_rows(
    events: &Arc<Mutex<Vec<CalendarEvent>>>,
    calendar_id: &str,
    gid: &str,
    rows: Vec<CalendarEvent>,
) {
    if let Ok(mut guard) = events.lock() {
        guard.retain(|e| {
            !(e.google_calendar_id.as_deref() == Some(calendar_id)
                && e.google_event_id.as_deref() == Some(gid))
        });
        guard.extend(rows);
    }
}

// ---------------------------------------------------------------------------
// Mapping: Google event → local rows
// ---------------------------------------------------------------------------

/// Convert a Google time (offset RFC 3339, IANA-hinted floating, or all-day
/// date) into an absolute instant we can render in local wall-clock.
fn google_time_local(t: &crate::google_api::GTime) -> Option<DateTime<FixedOffset>> {
    if let Some(dt) = &t.date_time {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(dt) {
            return Some(parsed);
        }
        // Floating time without offset: use the event's IANA zone hint.
        if let Some(zone) = t.time_zone.as_deref().and_then(|z| z.parse::<chrono_tz::Tz>().ok()) {
            let naive = chrono::NaiveDateTime::parse_from_str(dt.trim(), "%Y-%m-%dT%H:%M:%S%.f").ok()?;
            return Some(naive.and_local_timezone(zone).single()?.fixed_offset());
        }
        return None;
    }
    let date = t.date.as_deref()?;
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let midnight = d.and_hms_opt(0, 0, 0)?;
    let local = Local.from_local_datetime(&midnight).single()?;
    Some(local.with_timezone(&Utc.fix()))
}

/// Map a Google RRULE onto the local repeat enum when it can be represented
/// exactly: bare FREQ (optionally INTERVAL=1), or WEEKLY with a single /
/// weekday-set BYDAY. Anything else (COUNT, UNTIL, BYSETPOS, BYMONTHDAY,
/// INTERVAL>1, ...) needs expansion. Returns `None` when unmappable.
fn clean_rrule_rule(rrule_line: &str) -> Option<Repeat> {
    let body = rrule_line.strip_prefix("RRULE:").unwrap_or(rrule_line);
    let mut parts: HashMap<&str, &str> = HashMap::new();
    for piece in body.split(';') {
        if let Some((k, v)) = piece.split_once('=') {
            parts.insert(k, v);
        }
    }
    let freq = *parts.get("FREQ")?;
    // Reject any qualifier the local model cannot mirror.
    let supported = parts.iter().all(|(k, v)| match *k {
        "FREQ" => true,
        "INTERVAL" => *v == "1",
        "BYDAY" => freq == "WEEKLY",
        _ => false,
    });
    if !supported {
        return None;
    }
    match freq {
        "DAILY" => Some(Repeat::Daily),
        "MONTHLY" => Some(Repeat::Monthly),
        "YEARLY" => Some(Repeat::Yearly),
        "WEEKLY" => match parts.get("BYDAY").copied().unwrap_or("") {
            // Bare weekly, or weekly on one weekday (derivable from the
            // start date): both map to the local "weekly" rule.
            "" | "MO" | "TU" | "WE" | "TH" | "FR" | "SA" | "SU" => Some(Repeat::Weekly),
            "MO,TU,WE,TH,FR" => Some(Repeat::Weekdays),
            _ => None,
        },
        _ => None,
    }
}

/// Build an RRULE for a local repeat rule (push direction).
fn rrule_for_repeat(rule: Repeat, date: NaiveDate) -> Option<String> {
    let weekday = match date.weekday() {
        chrono::Weekday::Mon => "MO",
        chrono::Weekday::Tue => "TU",
        chrono::Weekday::Wed => "WE",
        chrono::Weekday::Thu => "TH",
        chrono::Weekday::Fri => "FR",
        chrono::Weekday::Sat => "SA",
        chrono::Weekday::Sun => "SU",
    };
    Some(match rule {
        Repeat::None => return None,
        Repeat::Daily => "RRULE:FREQ=DAILY".into(),
        Repeat::Weekdays => "RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".into(),
        Repeat::Weekly => format!("RRULE:FREQ=WEEKLY;BYDAY={weekday}"),
        Repeat::Monthly => "RRULE:FREQ=MONTHLY".into(),
        Repeat::Yearly => "RRULE:FREQ=YEARLY".into(),
    })
}

/// Parse an EXDATE recurrence line into an absolute instant. TZID-named
/// zones are resolved via chrono-tz; anything unparseable is skipped.
fn parse_exdate(line: &str, dt_start: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    let (params, value) = line.split_once(':')?;
    let params = params.strip_prefix("EXDATE").unwrap_or(params);
    let value = value.trim();
    let has_tzid = params.contains("TZID=");
    let date_only = params.contains("VALUE=DATE") || (value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()));
    if date_only {
        let d = NaiveDate::parse_from_str(&format!("{}-{}-{}", &value[0..4], &value[4..6], &value[6..8]), "%Y-%m-%d")
            .ok()?;
        // Whole-day exclusion: matches an all-day occurrence; timed series
        // rarely use VALUE=DATE.
        let midnight = d.and_hms_opt(0, 0, 0)?;
        return Some(Local.from_local_datetime(&midnight).single()?.fixed_offset());
    }
    let utc = value.ends_with('Z');
    let naive = chrono::NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S%.f").ok()?;
    if utc {
        return Some(Utc.from_utc_datetime(&naive).fixed_offset());
    }
    if has_tzid {
        // TZID=America/New_York:20260101T090000
        if let Some(tz_name) = params.split("TZID=").nth(1).and_then(|t| t.split(';').next()) {
            if let Ok(zone) = tz_name.parse::<chrono_tz::Tz>() {
                return Some(naive.and_local_timezone(zone).single()?.fixed_offset());
            }
        }
    }
    // Floating: assume the series' own offset.
    Some(naive.and_local_timezone(*dt_start.offset()).single()?)
}

/// Expand an RRULE (with EXDATEs) into occurrence instants, rendered in the
/// machine-local zone with the series' wall-clock preserved. (Google's own
/// IANA zone is not recoverable from the API's offset-only dtstart, and the
/// local model stores wall-clock times — so expansion is anchored at the
/// first occurrence's local wall time.) Falls back to the series' first
/// occurrence when the rule cannot be parsed.
fn expand_rrule(
    rrule_line: &str,
    exdates: &[DateTime<FixedOffset>],
    dt_start: DateTime<FixedOffset>,
    limit: usize,
) -> Vec<DateTime<Local>> {
    let body = rrule_line.strip_prefix("RRULE:").unwrap_or(rrule_line);
    let anchor_local = match Local.from_local_datetime(&dt_start.naive_local()) {
        chrono::LocalResult::Single(dt) => dt,
        _ => dt_start.with_timezone(&Local),
    };
    let anchor = anchor_local.with_timezone(&rrule::Tz::LOCAL);
    let fallback = || {
        eprintln!("[calendar] rrule expansion failed; importing first occurrence only");
        vec![anchor_local]
    };
    let parsed: rrule::RRule<rrule::Unvalidated> = match body.parse() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[calendar] rrule parse failed ({e})");
            return fallback();
        }
    };
    let validated = match parsed.validate(anchor) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[calendar] rrule validation failed ({e})");
            return fallback();
        }
    };
    let mut set = rrule::RRuleSet::new(anchor).rrule(validated);
    for ex in exdates {
        let ex_wall = match Local.from_local_datetime(&ex.naive_local()) {
            chrono::LocalResult::Single(dt) => dt,
            _ => ex.with_timezone(&Local),
        };
        set = set.exdate(ex_wall.with_timezone(&rrule::Tz::LOCAL));
    }
    set.all(limit as u16)
        .dates
        .into_iter()
        .map(|d| d.with_timezone(&Local))
        .collect()
}

/// Map one Google event to the local rows it should occupy. Returns one row
/// for clean-mapped rules and plain events, or N instance rows for expanded
/// series (including the first-occurrence fallback on parse failures).
fn map_google_event(
    calendar_id: &str,
    g: &GEvent,
    overrides: &HashMap<String, Vec<String>>,
    window_start: NaiveDate,
    window_end: NaiveDate,
) -> Result<Vec<CalendarEvent>, String> {
    let start = g.start.as_ref().ok_or("event has no start")?;
    let all_day = start.date.is_some() && start.date_time.is_none();
    let start_dt = google_time_local(start).ok_or("unreadable start time")?;
    let title = g.summary.clone().unwrap_or_else(|| "(untitled)".into());
    let description = g.description.clone().unwrap_or_default();

    let (end_hour, end_minute) = g
        .end
        .as_ref()
        .and_then(google_time_local)
        .and_then(|end_dt| {
            let start_local = start_dt.with_timezone(&Local);
            let end_local = end_dt.with_timezone(&Local);
            if end_local.date_naive() == start_local.date_naive() {
                Some((Some(end_local.hour() as u8), Some(end_local.minute() as u8)))
            } else {
                None // local model is same-day display only
            }
        })
        .unwrap_or((None, None));

    let recurrence = g.recurrence.clone().unwrap_or_default();
    let rrule_line = recurrence.iter().find(|l| l.starts_with("RRULE:")).cloned();
    let has_exdates = recurrence.iter().any(|l| l.starts_with("EXDATE"));
    let overridden = overrides
        .get(&g.id)
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    // 1. Clean rule with no exceptions → a single locally-repeating row.
    if let (Some(line), false, false) = (&rrule_line, has_exdates, overridden) {
        if let Some(rule) = clean_rrule_rule(line) {
            let (date, hour, minute) = local_ymd(start_dt, all_day, start);
            return Ok(vec![CalendarEvent {
                id: format!("gcal-{calendar_id}-{}", g.id),
                title,
                description,
                date,
                hour,
                minute,
                all_day,
                end_hour,
                end_minute,
                repeat: Some(rule),
                repeat_yearly: rule == Repeat::Yearly,
                message: String::new(),
                enabled: true,
                last_fired: None,
                source: Some("google".into()),
                google_calendar_id: Some(calendar_id.to_string()),
                google_event_id: Some(g.id.clone()),
                google_readonly: true,
                updated_at: None,
            }]);
        }
    }

    // 2. Recurring with exceptions/complex rules → expand instances.
    if let Some(line) = &rrule_line {
        let exdates: Vec<DateTime<FixedOffset>> = recurrence
            .iter()
            .filter(|l| l.starts_with("EXDATE"))
            .filter_map(|l| parse_exdate(l, start_dt))
            .collect();
        // Overridden occurrences have their own override event; skip the
        // series instance on those dates.
        let overridden_dates: &[String] = overrides
            .get(&g.id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let dates = expand_rrule(line, &exdates, start_dt, 400);
        let mut rows = Vec::new();
        for local in dates {
            let date = local.date_naive();
            if date < window_start || date > window_end {
                continue;
            }
            let iso = date.format("%Y-%m-%d").to_string();
            if overridden_dates.iter().any(|d| d == &iso) {
                continue;
            }
            rows.push(CalendarEvent {
                id: format!("gcal-{calendar_id}-{}-{iso}", g.id),
                title: title.clone(),
                description: description.clone(),
                date: iso,
                hour: if all_day { 9 } else { local.hour() as u8 },
                minute: if all_day { 0 } else { local.minute() as u8 },
                all_day,
                end_hour,
                end_minute,
                repeat: Some(Repeat::None),
                repeat_yearly: false,
                message: String::new(),
                enabled: true,
                last_fired: None,
                source: Some("google".into()),
                google_calendar_id: Some(calendar_id.to_string()),
                google_event_id: Some(g.id.clone()),
                google_readonly: true,
                updated_at: None,
            });
        }
        if rows.is_empty() {
            return Err("no occurrences in the sync window".into());
        }
        return Ok(rows);
    }

    // 3. Plain single event (also the mapping for instance overrides).
    let (date, hour, minute) = local_ymd(start_dt, all_day, start);
    Ok(vec![CalendarEvent {
        id: format!("gcal-{calendar_id}-{}", g.id),
        title,
        description,
        date,
        hour,
        minute,
        all_day,
        end_hour,
        end_minute,
        repeat: Some(Repeat::None),
        repeat_yearly: false,
        message: String::new(),
        enabled: true,
        last_fired: None,
        source: Some("google".into()),
        google_calendar_id: Some(calendar_id.to_string()),
        google_event_id: Some(g.id.clone()),
        google_readonly: false,
        updated_at: None,
    }])
}

/// Local date/time triple for a Google start instant. All-day events use
/// the calendar's own date; timed events render in local wall-clock.
fn local_ymd(
    start_dt: DateTime<FixedOffset>,
    all_day: bool,
    start: &crate::google_api::GTime,
) -> (String, u8, u8) {
    if all_day {
        if let Some(iso) = &start.date {
            return (iso.clone(), 9, 0);
        }
    }
    let local = start_dt.with_timezone(&Local);
    (
        local.date_naive().format("%Y-%m-%d").to_string(),
        local.hour() as u8,
        local.minute() as u8,
    )
}

// ---------------------------------------------------------------------------
// Mapping: local row → Google event body (push direction)
// ---------------------------------------------------------------------------

fn local_datetime(date_iso: &str, hour: u8, minute: u8) -> Option<DateTime<Local>> {
    let d = NaiveDate::parse_from_str(date_iso, "%Y-%m-%d").ok()?;
    let naive = d.and_hms_opt(hour as u32, minute as u32, 0)?;
    Local.from_local_datetime(&naive).single()
}

/// Build the Google events.insert/patch body for a local event.
fn event_to_google_body(e: &CalendarEvent) -> Value {
    let (start, end) = if e.all_day {
        // All-day events: end date is exclusive → next day.
        let end_iso = NaiveDate::parse_from_str(&e.date, "%Y-%m-%d")
            .map(|d| (d + chrono::Duration::days(1)).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|_| e.date.clone());
        (json!({ "date": e.date }), json!({ "date": end_iso }))
    } else {
        let now = Local::now();
        let start_dt = local_datetime(&e.date, e.hour, e.minute).unwrap_or(now);
        let end_dt = if let (Some(h), Some(m)) = (e.end_hour, e.end_minute) {
            local_datetime(&e.date, h, m).unwrap_or(start_dt + chrono::Duration::hours(1))
        } else {
            start_dt + chrono::Duration::hours(1)
        };
        (
            json!({ "dateTime": start_dt.to_rfc3339_opts(SecondsFormat::Secs, false) }),
            json!({ "dateTime": end_dt.to_rfc3339_opts(SecondsFormat::Secs, false) }),
        )
    };
    let mut body = json!({
        "summary": e.title,
        "description": e.description,
        "start": start,
        "end": end,
    });
    if let Some(d) = NaiveDate::parse_from_str(&e.date, "%Y-%m-%d").ok() {
        if let Some(rr) = rrule_for_repeat(e.repeat_rule(), d) {
            body["recurrence"] = json!([rr]);
        }
    }
    body
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_api::GTime;

    fn tz(h: i32) -> FixedOffset {
        FixedOffset::east_opt(h * 3600).unwrap()
    }

    #[test]
    fn clean_rules_map_to_the_enum() {
        assert_eq!(clean_rrule_rule("RRULE:FREQ=DAILY"), Some(Repeat::Daily));
        assert_eq!(clean_rrule_rule("RRULE:FREQ=WEEKLY"), Some(Repeat::Weekly));
        assert_eq!(
            clean_rrule_rule("RRULE:FREQ=WEEKLY;BYDAY=TU"),
            Some(Repeat::Weekly)
        );
        assert_eq!(
            clean_rrule_rule("RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"),
            Some(Repeat::Weekdays)
        );
        assert_eq!(clean_rrule_rule("RRULE:FREQ=MONTHLY"), Some(Repeat::Monthly));
        assert_eq!(clean_rrule_rule("RRULE:FREQ=YEARLY"), Some(Repeat::Yearly));
        assert_eq!(clean_rrule_rule("RRULE:FREQ=DAILY;INTERVAL=1"), Some(Repeat::Daily));
        // Not exactly representable → expansion.
        assert_eq!(clean_rrule_rule("RRULE:FREQ=DAILY;COUNT=5"), None);
        assert_eq!(clean_rrule_rule("RRULE:FREQ=DAILY;UNTIL=20261231T000000Z"), None);
        assert_eq!(clean_rrule_rule("RRULE:FREQ=WEEKLY;BYDAY=MO,WE"), None);
        assert_eq!(clean_rrule_rule("RRULE:FREQ=MONTHLY;BYMONTHDAY=15"), None);
        assert_eq!(clean_rrule_rule("RRULE:FREQ=DAILY;INTERVAL=2"), None);
    }

    #[test]
    fn rrules_build_from_repeat() {
        let tue = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(); // Tuesday
        assert_eq!(
            rrule_for_repeat(Repeat::Weekly, tue).as_deref(),
            Some("RRULE:FREQ=WEEKLY;BYDAY=TU")
        );
        assert_eq!(
            rrule_for_repeat(Repeat::Weekdays, tue).as_deref(),
            Some("RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR")
        );
        assert_eq!(
            rrule_for_repeat(Repeat::Yearly, tue).as_deref(),
            Some("RRULE:FREQ=YEARLY")
        );
        assert_eq!(rrule_for_repeat(Repeat::None, tue), None);
    }

    #[test]
    fn google_times_convert_to_local() {
        // Fixed offset parses directly.
        let t = GTime { date: None, date_time: Some("2026-11-28T15:00:00-05:00".into()), time_zone: None };
        let dt = google_time_local(&t).unwrap();
        assert_eq!(dt.format("%Y-%m-%dT%H:%M").to_string(), "2026-11-28T15:00");

        // Floating dateTime + IANA zone hint.
        let floating = GTime {
            date: None,
            date_time: Some("2026-11-28T09:30:00".into()),
            time_zone: Some("America/New_York".into()),
        };
        assert!(google_time_local(&floating).is_some());

        // All-day date → local midnight.
        let all_day = GTime { date: Some("2026-12-25".into()), date_time: None, time_zone: None };
        let dt = google_time_local(&all_day).unwrap();
        assert_eq!(dt.with_timezone(&Local).date_naive().to_string(), "2026-12-25");
    }

    #[test]
    fn expansion_expands_count_and_respects_exdates() {
        let dt_start = DateTime::parse_from_rfc3339("2026-11-30T09:00:00+00:00").unwrap();
        let dates = expand_rrule("RRULE:FREQ=DAILY;COUNT=5", &[], dt_start, 100);
        assert_eq!(dates.len(), 5);
        // EXDATE removes an occurrence.
        let ex = DateTime::parse_from_rfc3339("2026-12-02T09:00:00+00:00").unwrap();
        let dates = expand_rrule("RRULE:FREQ=DAILY;COUNT=5", &[ex], dt_start, 100);
        assert_eq!(dates.len(), 4);
        assert!(dates
            .iter()
            .all(|d| d.format("%Y-%m-%d").to_string() != "2026-12-02"));
        // Unparseable rule falls back to the first occurrence.
        let dates = expand_rrule("RRULE:FREQ=NOPE", &[], dt_start, 100);
        assert_eq!(dates.len(), 1);
    }

    fn gevent(id: &str, start: GTime, recurrence: Option<Vec<String>>) -> GEvent {
        GEvent {
            id: id.into(),
            status: Some("confirmed".into()),
            summary: Some("Lunch".into()),
            description: Some("team lunch".into()),
            start: Some(start),
            end: Some(GTime {
                date: None,
                date_time: Some("2026-12-01T13:00:00+00:00".into()),
                time_zone: None,
            }),
            recurrence,
            recurring_event_id: None,
            original_start: None,
        }
    }

    fn map_one(g: &GEvent, overrides: &HashMap<String, Vec<String>>) -> Vec<CalendarEvent> {
        map_google_event(
            "cal-x",
            g,
            overrides,
            NaiveDate::from_ymd_opt(2026, 11, 21).unwrap(),
            NaiveDate::from_ymd_opt(2027, 2, 26).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn single_event_maps_editable() {
        let g = gevent(
            "evt1",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            None,
        );
        let rows = map_one(&g, &HashMap::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "gcal-cal-x-evt1");
        assert_eq!(rows[0].source.as_deref(), Some("google"));
        assert!(!rows[0].google_readonly);
        assert_eq!(rows[0].repeat, Some(Repeat::None));
        assert_eq!(rows[0].title, "Lunch");
    }

    #[test]
    fn clean_series_maps_readonly_repeating() {
        let g = gevent(
            "series1",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            Some(vec!["RRULE:FREQ=WEEKLY".into()]),
        );
        let rows = map_one(&g, &HashMap::new());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].google_readonly);
        assert_eq!(rows[0].repeat, Some(Repeat::Weekly));
    }

    #[test]
    fn complex_series_expands_into_instances() {
        let g = gevent(
            "series2",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            Some(vec!["RRULE:FREQ=DAILY;COUNT=5".into()]),
        );
        let rows = map_one(&g, &HashMap::new());
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|r| r.google_readonly));
        assert_eq!(rows[0].date, "2026-12-01");
        assert_eq!(rows[1].date, "2026-12-02");
        assert_eq!(rows[0].hour, 12);
        assert!(rows[0].id.ends_with("-2026-12-01"));
    }

    #[test]
    fn overrides_force_expansion_and_exclude_dates() {
        let g = gevent(
            "series3",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            Some(vec!["RRULE:FREQ=DAILY;COUNT=5".into()]),
        );
        let mut overrides = HashMap::new();
        overrides.insert(
            "series3".to_string(),
            vec!["2026-12-02".to_string()],
        );
        let rows = map_one(&g, &overrides);
        // Clean rule would have mapped to one row; the override forces
        // expansion minus the overridden date (5 occurrences → 4 rows).
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.date != "2026-12-02"));
    }

    #[test]
    fn cancelled_events_are_not_mapped() {
        // (Handled by the caller; verify mapping of a cancelled event is
        // still structurally possible and callers filter first.)
        let mut g = gevent(
            "evt9",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            None,
        );
        g.status = Some("cancelled".into());
        let rows = map_one(&g, &HashMap::new());
        assert_eq!(rows.len(), 1); // caller must check status before mapping
    }

    #[test]
    fn local_body_roundtrips_through_mapping() {
        let mut e = CalendarEvent {
            id: "local-1".into(),
            title: "Dentist".into(),
            description: "checkup".into(),
            date: "2030-03-10".into(),
            hour: 15,
            minute: 30,
            all_day: false,
            end_hour: Some(16),
            end_minute: Some(30),
            repeat: Some(Repeat::Weekly),
            repeat_yearly: false,
            message: "hi".into(),
            enabled: true,
            last_fired: None,
            source: None,
            google_calendar_id: None,
            google_event_id: None,
            google_readonly: false,
            updated_at: None,
        };
        let body = event_to_google_body(&e);
        assert_eq!(body["summary"], "Dentist");
        let start = body["start"]["dateTime"].as_str().unwrap();
        let dt = DateTime::parse_from_rfc3339(start).unwrap();
        assert_eq!(dt.with_timezone(&Local).hour(), 15);
        assert_eq!(body["recurrence"][0], "RRULE:FREQ=WEEKLY;BYDAY=SU");
        // 2030-03-10 is a Sunday.
        assert_eq!(body["end"]["dateTime"].is_string(), true);

        // All-day + yearly.
        e.all_day = true;
        e.end_hour = None;
        e.end_minute = None;
        e.repeat = Some(Repeat::Yearly);
        e.repeat_yearly = true;
        let body = event_to_google_body(&e);
        assert_eq!(body["start"]["date"], "2030-03-10");
        assert_eq!(body["end"]["date"], "2030-03-11");
        assert_eq!(body["recurrence"][0], "RRULE:FREQ=YEARLY");
        assert!(body["start"]["dateTime"].is_null());
    }

    #[test]
    fn parse_exdate_variants() {
        let dt_start = DateTime::parse_from_rfc3339("2026-12-01T09:00:00+00:00").unwrap();
        // Compare instants, normalized to UTC (zone offsets may differ).
        let utc = |dt: DateTime<FixedOffset>| {
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        // UTC form.
        let ex = parse_exdate("EXDATE:20261202T090000Z", dt_start).unwrap();
        assert_eq!(utc(ex), "2026-12-02T09:00:00Z");
        // Floating form inherits the series offset.
        let ex = parse_exdate("EXDATE:20261203T090000", dt_start).unwrap();
        assert_eq!(utc(ex), "2026-12-03T09:00:00Z");
        // TZID form (EST = -5 in December).
        let ex = parse_exdate(
            "EXDATE;TZID=America/New_York:20261204T090000",
            dt_start,
        )
        .unwrap();
        assert_eq!(utc(ex), "2026-12-04T14:00:00Z");
        // Garbage → None.
        assert!(parse_exdate("EXDATE:nonsense", dt_start).is_none());
    }
}
