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
//! - Every Google event — recurring or not — maps onto exactly one local
//!   row with the same shape as app-created events. RRULEs map onto the
//!   closest local `Repeat` rule; qualifiers the local model cannot
//!   represent (COUNT/UNTIL/EXDATE/overrides/complex BYDAY) are simplified
//!   away, so a series repeats forever locally and single-instance
//!   exceptions may not mirror Google exactly.
//! - All synced events are editable in the app and push back.
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
use std::collections::HashSet;
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

    /// Queue an upsert push for a locally-mutated event.
    pub fn enqueue_upsert(&self, e: &CalendarEvent) {
        if !self.connected() {
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
        if !self.connected() {
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
    for cal_id in &selected {
        pull_calendar(&api, &tokens.access_token, cal_id, events, store)?;
    }
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
    apply_pulled_events(calendar_id, &pulled, events, store);
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
) {
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
        let row = match map_google_event(calendar_id, g) {
            Ok(row) => row,
            Err(e) => {
                ctx_debug(calendar_id, &format!("skipping {}: {e}", g.id));
                continue;
            }
        };
        // Preserve app-only fields from any existing row for this Google
        // event — same mapped id, or an app-created row that was pushed and
        // now carries the Google ids.
        let mut merged = vec![row];
        for m in &mut merged {
            if let Some(old) = snapshot.iter().find(|x| {
                x.id == m.id
                    || (x.google_calendar_id.as_deref() == Some(calendar_id)
                        && x.google_event_id.as_deref() == Some(g.id.as_str()))
            }) {
                m.message = old.message.clone();
                m.enabled = old.enabled;
                m.last_fired = old.last_fired;
                m.updated_at = old.updated_at;
            }
        }
        // Replace any stored rows for this Google event (an app-created row
        // pushed to Google, or a differently-shaped row from an older
        // plugin version).
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

/// Map a Google RRULE onto the closest local repeat rule. The local model
/// has no COUNT/UNTIL/EXDATE support, so qualifiers are simplified away: a
/// series repeats forever from its start date, and single-instance
/// exceptions (EXDATEs, per-instance overrides) do not narrow it. BYDAY
/// sets beyond Mon–Fri anchor on the start date's weekday. Returns `None`
/// for rules without a usable FREQ (mapped as non-repeating).
fn best_effort_rule(rrule_line: &str) -> Option<Repeat> {
    let body = rrule_line.strip_prefix("RRULE:").unwrap_or(rrule_line);
    let mut freq: Option<&str> = None;
    let mut byday: Option<&str> = None;
    for piece in body.split(';') {
        if let Some((k, v)) = piece.split_once('=') {
            match k {
                "FREQ" => freq = Some(v),
                "BYDAY" => byday = Some(v),
                _ => {} // COUNT/UNTIL/INTERVAL/BYSETPOS/...: simplified away
            }
        }
    }
    match freq? {
        "DAILY" => Some(Repeat::Daily),
        "MONTHLY" => Some(Repeat::Monthly),
        "YEARLY" => Some(Repeat::Yearly),
        "WEEKLY" => match byday {
            Some("MO,TU,WE,TH,FR") => Some(Repeat::Weekdays),
            _ => Some(Repeat::Weekly),
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

/// Map one Google event to the single local row it occupies. Recurring
/// series map onto the closest local repeat rule (see `best_effort_rule`);
/// plain events and per-instance overrides map as non-repeating rows. All
/// mapped rows keep the exact same shape as app-created events and are
/// editable — edits push back to Google.
fn map_google_event(calendar_id: &str, g: &GEvent) -> Result<CalendarEvent, String> {
    let start = g.start.as_ref().ok_or("event has no start")?;
    let all_day = start.date.is_some() && start.date_time.is_none();
    let start_dt = google_time_local(start).ok_or("unreadable start time")?;
    let (date, hour, minute) = local_ymd(start_dt, all_day, start);
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
    let rule = recurrence
        .iter()
        .find(|l| l.starts_with("RRULE:"))
        .and_then(|line| best_effort_rule(line));

    Ok(CalendarEvent {
        id: format!("gcal-{calendar_id}-{}", g.id),
        title,
        description,
        date,
        hour,
        minute,
        all_day,
        end_hour,
        end_minute,
        repeat: Some(rule.unwrap_or(Repeat::None)),
        repeat_yearly: rule == Some(Repeat::Yearly),
        message: String::new(),
        enabled: true,
        last_fired: None,
        source: Some("google".into()),
        google_calendar_id: Some(calendar_id.to_string()),
        google_event_id: Some(g.id.clone()),
        updated_at: None,
    })
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

    #[test]
    fn rrules_map_to_the_closest_rule() {
        assert_eq!(best_effort_rule("RRULE:FREQ=DAILY"), Some(Repeat::Daily));
        assert_eq!(best_effort_rule("RRULE:FREQ=WEEKLY"), Some(Repeat::Weekly));
        assert_eq!(
            best_effort_rule("RRULE:FREQ=WEEKLY;BYDAY=TU"),
            Some(Repeat::Weekly)
        );
        assert_eq!(
            best_effort_rule("RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"),
            Some(Repeat::Weekdays)
        );
        assert_eq!(best_effort_rule("RRULE:FREQ=MONTHLY"), Some(Repeat::Monthly));
        assert_eq!(best_effort_rule("RRULE:FREQ=YEARLY"), Some(Repeat::Yearly));
        assert_eq!(best_effort_rule("RRULE:FREQ=DAILY;INTERVAL=1"), Some(Repeat::Daily));
        // Qualifiers the local model cannot represent are simplified away.
        assert_eq!(best_effort_rule("RRULE:FREQ=DAILY;COUNT=5"), Some(Repeat::Daily));
        assert_eq!(
            best_effort_rule("RRULE:FREQ=DAILY;UNTIL=20261231T000000Z"),
            Some(Repeat::Daily)
        );
        assert_eq!(best_effort_rule("RRULE:FREQ=WEEKLY;BYDAY=MO,WE"), Some(Repeat::Weekly));
        assert_eq!(
            best_effort_rule("RRULE:FREQ=MONTHLY;BYMONTHDAY=15"),
            Some(Repeat::Monthly)
        );
        assert_eq!(best_effort_rule("RRULE:FREQ=DAILY;INTERVAL=2"), Some(Repeat::Daily));
        // No usable FREQ → falls back to a non-repeating row.
        assert_eq!(best_effort_rule("RRULE:FREQ=NOPE"), None);
        assert_eq!(best_effort_rule("RRULE:BOGUS"), None);
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
        }
    }

    fn map_one(g: &GEvent) -> CalendarEvent {
        map_google_event("cal-x", g).unwrap()
    }

    #[test]
    fn single_event_maps_editable() {
        let g = gevent(
            "evt1",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            None,
        );
        let row = map_one(&g);
        assert_eq!(row.id, "gcal-cal-x-evt1");
        assert_eq!(row.source.as_deref(), Some("google"));
        assert_eq!(row.repeat, Some(Repeat::None));
        assert_eq!(row.title, "Lunch");
        // Same shape as a local event: enabled, editable, no extras.
        assert!(row.enabled);
        assert_eq!(row.message, "");
    }

    #[test]
    fn series_maps_to_repeating_row() {
        let g = gevent(
            "series1",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            Some(vec!["RRULE:FREQ=WEEKLY".into()]),
        );
        let row = map_one(&g);
        assert_eq!(row.repeat, Some(Repeat::Weekly));
        assert!(row.repeat_yearly == false);
    }

    #[test]
    fn complex_series_simplifies_to_nearest_rule() {
        // COUNT/UNTIL/EXDATE qualifiers are simplified away: the series
        // becomes one locally-repeating row, not per-occurrence instances.
        let g = gevent(
            "series2",
            GTime { date: None, date_time: Some("2026-12-01T12:00:00+00:00".into()), time_zone: None },
            Some(vec!["RRULE:FREQ=DAILY;COUNT=5".into()]),
        );
        let row = map_one(&g);
        assert_eq!(row.id, "gcal-cal-x-series2");
        // Google times render in local wall-clock (UTC noon here).
        let start_local = DateTime::parse_from_rfc3339("2026-12-01T12:00:00+00:00")
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(row.date, start_local.date_naive().format("%Y-%m-%d").to_string());
        assert_eq!(row.hour, start_local.hour() as u8);
        assert_eq!(row.minute, start_local.minute() as u8);
        assert_eq!(row.repeat, Some(Repeat::Daily));
    }

    #[test]
    fn yearly_series_maps_yearly() {
        let g = gevent(
            "series3",
            GTime { date: Some("2026-12-25".into()), date_time: None, time_zone: None },
            Some(vec!["RRULE:FREQ=YEARLY".into()]),
        );
        let row = map_one(&g);
        assert!(row.all_day);
        assert_eq!(row.hour, 9);
        assert_eq!(row.repeat, Some(Repeat::Yearly));
        assert!(row.repeat_yearly);
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
        let row = map_one(&g); // caller must check status before mapping
        assert_eq!(row.title, "Lunch");
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
}
