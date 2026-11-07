//! Calendar feature plugin backend.
//!
//! Port of the former host module `src-tauri/src/calendar.rs` plus the
//! model-facing calendar tools from the former builtin MCP server in `mcp.rs`.
//!
//! - Storage: `{data_dir}/calendar.json` (migrated from the legacy
//!   `{app_data}/calendar.json` by the host).
//! - Scheduler: a plain 20s thread; on fire it emits the namespaced event
//!   `plugin:calendar:alarm-fired` with the same payload shape alarms use, so
//!   reminder banners speak the same protocol.
//! - Tools: `calendar_list/create/update/delete` reach the chat model as
//!   `calendar__<name>`.

use chrono::{Datelike, Local, NaiveDate, Timelike};
use companion_plugin_sdk::{export_plugin, Plugin, PluginCtx, ToolDef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Google-style repeat rule. `weekly`/`monthly`/`yearly` derive their
/// pattern from the event's start date ("Weekly on Tuesday").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    None,
    Daily,
    Weekdays,
    Weekly,
    Monthly,
    Yearly,
}

/// A dated event: fires at date + clock, optionally repeating.
/// All-day events fire at 09:00 local. `end_hour`/`end_minute`
/// are display-only — they never trigger a second fire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// ISO date "YYYY-MM-DD" — the first occurrence.
    #[serde(default)]
    pub date: String,
    #[serde(default = "default_nine")]
    pub hour: u8,
    #[serde(default)]
    pub minute: u8,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub end_hour: Option<u8>,
    #[serde(default)]
    pub end_minute: Option<u8>,
    /// Repeat rule. Absent on old files: falls back to `repeat_yearly`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<Repeat>,
    /// Legacy flag kept in sync (`rule == Yearly`) so old app versions can
    /// still read files written by this one.
    #[serde(default)]
    pub repeat_yearly: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minutes since epoch of the last fire, so a tick never double-fires.
    #[serde(default)]
    pub last_fired: Option<i64>,
}

impl CalendarEvent {
    /// The effective repeat rule, mapping old files that only carry the
    /// legacy `repeat_yearly` bool.
    pub fn repeat_rule(&self) -> Repeat {
        self.repeat.unwrap_or(if self.repeat_yearly {
            Repeat::Yearly
        } else {
            Repeat::None
        })
    }
}

fn default_true() -> bool {
    true
}

fn default_nine() -> u8 {
    9
}

struct Calendar {
    events: Arc<Mutex<Vec<CalendarEvent>>>,
    data_dir: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Calendar {
    fn load(&mut self) {
        let path = self.data_dir.join("calendar.json");
        let list: Vec<CalendarEvent> = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if let Ok(mut guard) = self.events.lock() {
            *guard = list;
        }
    }

    fn write(&self, list: &[CalendarEvent]) -> Result<(), String> {
        let path = self.data_dir.join("calendar.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("write calendar: {e}"))
    }

    fn persist(&self) -> Result<Vec<CalendarEvent>, String> {
        let list = self.events.lock().map_err(|e| e.to_string())?.clone();
        self.write(&list)?;
        Ok(list)
    }

    fn op_save(&self, event: CalendarEvent) -> Result<Vec<CalendarEvent>, String> {
        let mut e = event;
        sanitize(&mut e)?;
        let mut guard = self.events.lock().map_err(|e| e.to_string())?;
        match guard.iter().position(|x| x.id == e.id) {
            Some(i) => guard[i] = e,
            None => guard.push(e),
        }
        drop(guard);
        self.persist()
    }

    fn op_delete(&self, id: &str) -> Result<Vec<CalendarEvent>, String> {
        let mut guard = self.events.lock().map_err(|e| e.to_string())?;
        guard.retain(|e| e.id != id);
        drop(guard);
        self.persist()
    }

    fn op_toggle(&self, id: &str, enabled: bool) -> Result<Vec<CalendarEvent>, String> {
        {
            let mut guard = self.events.lock().map_err(|e| e.to_string())?;
            if let Some(e) = guard.iter_mut().find(|e| e.id == id) {
                e.enabled = enabled;
                if enabled {
                    e.last_fired = None;
                }
            }
        }
        self.persist()
    }

    // -- model-facing tools (former builtin MCP server) ---------------------

    fn tool_list(&self) -> Result<String, String> {
        let list = self.events.lock().map_err(|e| e.to_string())?.clone();
        if list.is_empty() {
            return Ok("No calendar events.".into());
        }
        let now = Local::now();
        Ok(list
            .iter()
            .map(|e| format_calendar_line(e, now))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn tool_create(&self, args: &Value) -> Result<String, String> {
        let date_text = [
            arg_str(args, "date"),
            arg_str(args, "on"),
            arg_str(args, "at"),
        ]
        .iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default();
        let date = parse_date(&date_text).map(|d| d.format("%Y-%m-%d").to_string());
        let Some(date) = date else {
            return Err("could not read the date. Use YYYY-MM-DD, 'today', 'tomorrow', 'Dec 25', or a weekday name.".into());
        };
        let (hour, minute) = match (
            args.get("hour").and_then(|v| v.as_u64()),
            args.get("minute").and_then(|v| v.as_u64()),
        ) {
            (Some(h), Some(m)) if h <= 23 && m <= 59 => (h as u8, m as u8),
            _ => {
                let text = [arg_str(args, "time")]
                    .iter()
                    .find(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_default();
                match parse_clock_text(&text) {
                    Some((h, m)) => (h, m),
                    None if args
                        .get("all_day")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false) =>
                    {
                        (9, 0)
                    }
                    None => return Err(
                        "could not read the time. Use hour and minute, or a string like '15:00'."
                            .into(),
                    ),
                }
            }
        };
        let (end_hour, end_minute) = match parse_clock_text(&arg_str(args, "end")) {
            Some((h, m)) => (Some(h), Some(m)),
            None => (None, None),
        };
        let repeat = match arg_str(args, "repeat").to_lowercase().as_str() {
            "daily" => Some(Repeat::Daily),
            "weekdays" => Some(Repeat::Weekdays),
            "weekly" => Some(Repeat::Weekly),
            "monthly" => Some(Repeat::Monthly),
            "yearly" | "year" | "annually" => Some(Repeat::Yearly),
            "none" | "" => None,
            other => return Err(format!(
                "unknown repeat '{other}'. Use none, daily, weekdays, weekly, monthly, or yearly."
            )),
        };
        let event = CalendarEvent {
            id: String::new(),
            title: arg_str(args, "title").trim().to_string(),
            description: arg_str(args, "description"),
            date,
            hour,
            minute,
            all_day: args
                .get("all_day")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            end_hour,
            end_minute,
            repeat,
            repeat_yearly: args
                .get("repeat_yearly")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            message: arg_str(args, "message"),
            enabled: true,
            last_fired: None,
        };
        let title = if event.title.is_empty() {
            "Event".to_string()
        } else {
            event.title.clone()
        };
        let when = event.date.clone();
        self.op_save(event)?;
        Ok(format!("Created event '{title}' on {when}."))
    }

    fn tool_update(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "id");
        if id.is_empty() {
            return Err("id is required. Call calendar_list to find it.".into());
        }
        let mut e = {
            let guard = self.events.lock().map_err(|e| e.to_string())?;
            guard
                .iter()
                .find(|e| e.id == id)
                .cloned()
                .ok_or("no event with that id. Call calendar_list to find it.")?
        };
        // Partial patch: only fields present in the args change.
        if args.get("date").or_else(|| args.get("on")).is_some() {
            let text = [arg_str(args, "date"), arg_str(args, "on")]
                .iter()
                .find(|s| !s.is_empty())
                .cloned()
                .unwrap_or_default();
            match parse_date(&text) {
                Some(d) => e.date = d.format("%Y-%m-%d").to_string(),
                None => return Err("could not read the date. Use YYYY-MM-DD.".into()),
            }
        }
        if args.get("hour").is_some() || args.get("minute").is_some() {
            let text = arg_str(args, "time");
            let text_clock = parse_clock_text(&text);
            let h = args
                .get("hour")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or_else(|| text_clock.map(|(h, _)| h as u32));
            let m = args
                .get("minute")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or_else(|| text_clock.map(|(_, m)| m as u32));
            match (h, m) {
                (Some(h), Some(m)) if h <= 23 && m <= 59 => {
                    e.hour = h as u8;
                    e.minute = m as u8;
                }
                (Some(h), _) if h > 23 => return Err("hour must be 0-23.".into()),
                (_, Some(m)) if m > 59 => return Err("minute must be 0-59.".into()),
                _ => return Err("pass both hour and minute (0-23, 0-59).".into()),
            }
        }
        if let Some(v) = args.get("all_day").and_then(|v| v.as_bool()) {
            e.all_day = v;
        }
        if args.get("end").is_some() {
            match parse_clock_text(&arg_str(args, "end")) {
                Some((h, m)) => {
                    e.end_hour = Some(h);
                    e.end_minute = Some(m);
                }
                None => {
                    e.end_hour = None;
                    e.end_minute = None;
                }
            }
        }
        if let Some(v) = args.get("repeat_yearly").and_then(|v| v.as_bool()) {
            e.repeat_yearly = v;
        }
        if args.get("repeat").is_some() {
            let rule = match arg_str(args, "repeat").to_lowercase().as_str() {
                "daily" => Some(Repeat::Daily),
                "weekdays" => Some(Repeat::Weekdays),
                "weekly" => Some(Repeat::Weekly),
                "monthly" => Some(Repeat::Monthly),
                "yearly" | "year" | "annually" => Some(Repeat::Yearly),
                "none" | "" => None,
                other => {
                    return Err(format!(
                        "unknown repeat '{other}'. Use none, daily, weekdays, weekly, monthly, or yearly."
                    ))
                }
            };
            e.repeat = rule;
        }
        if args.get("title").is_some() {
            e.title = arg_str(args, "title");
        }
        if args.get("description").is_some() {
            e.description = arg_str(args, "description");
        }
        if args.get("message").is_some() {
            e.message = arg_str(args, "message");
        }
        if let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) {
            e.enabled = enabled;
            if enabled {
                e.last_fired = None;
            }
        }
        self.op_save(e.clone())?;
        Ok(format!("Updated event on {}.", e.date))
    }

    fn tool_delete(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "id");
        if id.is_empty() {
            return Err("id is required. Call calendar_list to find it.".into());
        }
        self.op_delete(&id)?;
        Ok("Deleted that event.".into())
    }

    fn handle_tool(&self, ctx: &PluginCtx, name: &str, args: Value) -> Result<Value, String> {
        let result = match name {
            "calendar_list" => self.tool_list(),
            "calendar_create" => self.tool_create(&args),
            "calendar_update" => self.tool_update(&args),
            "calendar_delete" => self.tool_delete(&args),
            other => return Err(format!("unknown tool: {other}")),
        }?;
        ctx.info(&format!("tool {name}"));
        Ok(Value::String(result))
    }

    fn route(&self, ctx: &PluginCtx, method: &str, payload: Value) -> Result<Value, String> {
        match method {
            "calendar.list" => {
                let list = self.events.lock().map_err(|e| e.to_string())?.clone();
                Ok(json!({ "events": list }))
            }
            "calendar.save" => {
                let event: CalendarEvent =
                    serde_json::from_value(payload.get("event").cloned().unwrap_or(Value::Null))
                        .map_err(|e| format!("bad event: {e}"))?;
                let list = self.op_save(event)?;
                Ok(json!({ "events": list }))
            }
            "calendar.delete" => {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("id is required")?;
                let list = self.op_delete(id)?;
                Ok(json!({ "events": list }))
            }
            "calendar.toggle" => {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("id is required")?;
                let enabled = payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or("enabled is required")?;
                let list = self.op_toggle(id, enabled)?;
                Ok(json!({ "events": list }))
            }
            "tool" => {
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("tool name missing")?;
                let args = payload.get("args").cloned().unwrap_or(Value::Null);
                self.handle_tool(ctx, name, args)
            }
            "calendar.ping" => Ok(json!({ "pong": true })),
            other => Err(format!("unknown method: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Date logic (ported from calendar.rs; pure helpers stay testable)
// ---------------------------------------------------------------------------

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    // Strict ISO first.
    if let Ok(d) = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
        return Some(d);
    }
    let lower = s.trim().to_lowercase();
    let today = Local::now().date_naive();
    match lower.as_str() {
        "today" => return Some(today),
        "tomorrow" => return Some(today + chrono::Duration::days(1)),
        _ => {}
    }
    // "Dec 25" / "25 Dec" with optional year: assume this year, roll to
    // next year when the date already passed.
    let months = [
        ("jan", 1u32),
        ("feb", 2),
        ("mar", 3),
        ("apr", 4),
        ("may", 5),
        ("jun", 6),
        ("jul", 7),
        ("aug", 8),
        ("sep", 9),
        ("oct", 10),
        ("nov", 11),
        ("dec", 12),
    ];
    let mut month: Option<u32> = None;
    let mut day: Option<u32> = None;
    for token in lower.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        let key = if token.len() > 3 { &token[..3] } else { token };
        if let Some((_, m)) = months.iter().find(|(name, _)| *name == key) {
            month = Some(*m);
        } else if let Ok(n) = token.parse::<u32>() {
            if token.len() == 4 && n >= 1900 && n <= 2200 {
                // Explicit year: require a full ISO match instead.
                return NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok();
            }
            if n >= 1 && n <= 31 && day.is_none() {
                day = Some(n);
            }
        }
    }
    match (month, day) {
        (Some(m), Some(d)) => {
            let year = today.year();
            let cand = NaiveDate::from_ymd_opt(year, m, d)?;
            if cand < today {
                NaiveDate::from_ymd_opt(year + 1, m, d)
            } else {
                Some(cand)
            }
        }
        _ => {
            // Weekday name: next occurrence (never today-past ambiguity —
            // same-day names mean the upcoming one, 7 days out if passed).
            let days = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
            let key = if lower.len() > 3 {
                &lower[..3]
            } else {
                lower.as_str()
            };
            days.iter().position(|d| *d == key).and_then(|target| {
                let today_idx = today.weekday().num_days_from_monday() as usize;
                let mut offset = (target + 7 - today_idx) % 7;
                if offset == 0 {
                    offset = 7;
                }
                Some(today + chrono::Duration::days(offset as i64))
            })
        }
    }
}

fn sanitize(ev: &mut CalendarEvent) -> Result<(), String> {
    let d = parse_date(&ev.date)
        .ok_or_else(|| "could not read the date. Use YYYY-MM-DD, 'today', 'tomorrow', 'Dec 25', or a weekday name.".to_string())?;
    let today = Local::now().date_naive();
    // Only one-shots must start in the future; a recurring event may begin
    // in the past (its upcoming occurrences are what fire).
    if ev.repeat_rule() == Repeat::None && d < today {
        return Err(
            "that date is in the past. Pick today or a future date, or set a repeat option.".into(),
        );
    }
    ev.date = d.format("%Y-%m-%d").to_string();
    let rule = ev.repeat_rule();
    ev.repeat = Some(rule);
    ev.repeat_yearly = rule == Repeat::Yearly;
    if ev.all_day {
        ev.hour = 9;
        ev.minute = 0;
    } else {
        ev.hour = ev.hour.min(23);
        ev.minute = ev.minute.min(59);
    }
    if let Some(h) = ev.end_hour {
        ev.end_hour = Some(h.min(23));
    }
    if let Some(m) = ev.end_minute {
        ev.end_minute = Some(m.min(59));
    }
    ev.title = ev.title.trim().to_string();
    ev.description = ev.description.trim().to_string();
    ev.message = ev.message.trim().to_string();
    if ev.id.trim().is_empty() {
        ev.id = format!(
            "event-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
    }
    Ok(())
}

fn event_date(ev: &CalendarEvent) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(&ev.date, "%Y-%m-%d").ok()
}

/// True when the event has an occurrence on `date`. Yearly keeps its
/// birthday-style any-year semantics; other rules only run from the start
/// date forward (monthly dates that don't exist in a month simply skip).
fn occurs_on(ev: &CalendarEvent, date: NaiveDate) -> bool {
    let Some(d) = event_date(ev) else {
        return false;
    };
    match ev.repeat_rule() {
        Repeat::None => d == date,
        Repeat::Daily => d <= date,
        Repeat::Weekdays => {
            d <= date && !matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun)
        }
        Repeat::Weekly => d <= date && d.weekday() == date.weekday(),
        Repeat::Monthly => d <= date && d.day() == date.day(),
        Repeat::Yearly => d.month() == date.month() && d.day() == date.day(),
    }
}

/// True when `ev` should fire at `now`'s minute.
fn is_due(ev: &CalendarEvent, now: chrono::DateTime<Local>, minute_stamp: i64) -> bool {
    if !ev.enabled {
        return false;
    }
    if !occurs_on(ev, now.date_naive()) {
        return false;
    }
    let (h, m) = if ev.all_day {
        (9, 0)
    } else {
        (ev.hour, ev.minute)
    };
    if h != now.hour() as u8 || m != now.minute() as u8 {
        return false;
    }
    ev.last_fired != Some(minute_stamp)
}

fn tick(
    events: &Mutex<Vec<CalendarEvent>>,
    write: &dyn Fn(&[CalendarEvent]) -> Result<(), String>,
) -> Vec<FiredEvent> {
    let now = Local::now();
    let minute_stamp = now.timestamp() / 60;
    let mut fired: Vec<FiredEvent> = Vec::new();
    if let Ok(mut guard) = events.lock() {
        let mut changed = false;
        for ev in guard.iter_mut() {
            if !is_due(ev, now, minute_stamp) {
                continue;
            }
            ev.last_fired = Some(minute_stamp);
            // One-shots retire after firing; recurring events stay armed.
            if ev.repeat_rule() == Repeat::None {
                ev.enabled = false;
            }
            let label = if ev.title.trim().is_empty() {
                "Calendar event".to_string()
            } else {
                ev.title.clone()
            };
            fired.push(FiredEvent {
                id: ev.id.clone(),
                label,
                message: ev.message.clone(),
                loop_sound: false,
            });
            changed = true;
        }
        if changed {
            let snapshot = guard.clone();
            if let Err(e) = write(&snapshot) {
                eprintln!("[calendar] persist failed: {e}");
            }
        }
    }
    fired
}

/// Same wire shape as the alarms plugin's `alarm-fired` payload, so reminder
/// presentation is protocol-compatible across the two plugins.
#[derive(Clone, Serialize)]
pub struct FiredEvent {
    pub id: String,
    pub label: String,
    pub message: String,
    pub loop_sound: bool,
}

fn spawn_scheduler(
    ctx: PluginCtx,
    events: Arc<Mutex<Vec<CalendarEvent>>>,
    data_dir: PathBuf,
    stop: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("calendar-plugin-scheduler".into())
        .spawn(move || {
            // 20s tick: same cadence as the alarm scheduler.
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(20));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let write = |list: &[CalendarEvent]| -> Result<(), String> {
                    let path = data_dir.join("calendar.json");
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let json = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
                    std::fs::write(&path, json).map_err(|e| format!("write calendar: {e}"))
                };
                for f in tick(&events, &write) {
                    ctx.info(&format!("fired {}", f.id));
                    ctx.emit(
                        "alarm-fired",
                        &serde_json::to_value(&f).unwrap_or(Value::Null),
                    );
                    ctx.emit("calendar-changed", &Value::Null);
                }
            }
        })
        .expect("spawn calendar scheduler thread");
}

// ---------------------------------------------------------------------------
// Model-tool helpers (subset ported from the former builtin MCP server)
// ---------------------------------------------------------------------------

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Lenient "7am" / "7:30" / "19:05" parser.
fn parse_clock_text(s: &str) -> Option<(u8, u8)> {
    let lower = s.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    let mer = if lower.ends_with("am") {
        Some("am")
    } else if lower.ends_with("pm") {
        Some("pm")
    } else {
        None
    };
    let body = match mer {
        Some(_) => lower[..lower.len() - 2].trim(),
        None => lower.as_str(),
    };
    let (h_str, m_str) = match body.split_once(':') {
        Some((h, m)) => (h.trim(), Some(m.trim())),
        None => (body, None),
    };
    let mut h: u32 = h_str.parse().ok()?;
    let m: u32 = match m_str {
        Some(s) => s.parse().ok()?,
        None => 0,
    };
    match mer {
        Some("pm") if h < 12 => h += 12,
        Some("am") if h == 12 => h = 0,
        _ => {}
    }
    if h > 23 || m > 59 {
        return None;
    }
    Some((h as u8, m as u8))
}

/// "10:00 PM" — models read a 12-hour clock back to the user far more
/// naturally than 22:00.
fn human_clock(hour: u8, minute: u8) -> String {
    let (h12, suffix) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour as u32, "AM"),
        12 => (12, "PM"),
        _ => (hour as u32 - 12, "PM"),
    };
    format!("{h12}:{minute:02} {suffix}")
}

/// One human-readable line per calendar event. Same id-fencing rule as
/// alarms: the model must never read the id back to the user.
fn format_calendar_line(e: &CalendarEvent, now: chrono::DateTime<Local>) -> String {
    let title = if e.title.trim().is_empty() {
        "Event"
    } else {
        e.title.trim()
    };
    let when = match NaiveDate::parse_from_str(&e.date, "%Y-%m-%d") {
        Ok(d) => {
            let date_part = if e.repeat_yearly {
                format!("every {}", d.format("%b %d"))
            } else {
                let days = (d - now.date_naive()).num_days();
                match days {
                    0 => "today".into(),
                    1 => "tomorrow".into(),
                    2..=6 => d.format("%A").to_string(),
                    _ => d.format("%b %d, %Y").to_string(),
                }
            };
            let clock = if e.all_day {
                "all day".to_string()
            } else {
                human_clock(e.hour, e.minute)
            };
            let end = match (e.end_hour, e.end_minute) {
                (Some(h), Some(m)) => format!(" – ends {}", human_clock(h, m)),
                _ => String::new(),
            };
            format!("{date_part} at {clock}{end}")
        }
        Err(_) => e.date.clone(),
    };
    let status = if !e.enabled { "paused" } else { "active" };
    format!(
        "- {title} — {when} ({status}) [id_for_tool_use_only: {}]",
        e.id
    )
}

fn tool_defs() -> Vec<ToolDef> {
    let schema = |props: Value, required: &[&str]| {
        json!({
            "type": "object",
            "properties": props,
            "required": required,
        })
    };
    vec![
        ToolDef {
            name: "calendar_list".into(),
            description: "List the user's calendar events as a human-readable summary: title, date, time, and whether active or past. When you answer the user, describe events in words only (e.g. 'your dentist appointment on Friday'). Never read the internal id aloud — it exists only as the `id` argument for calendar_update or calendar_delete.".into(),
            input_schema: schema(json!({}), &[]),
        },
        ToolDef {
            name: "calendar_create".into(),
            description: "Create a calendar event for a specific date. Use for requests like 'remind me on Dec 25' or 'dentist appointment next Friday at 3pm'. Date accepts YYYY-MM-DD, 'today', 'tomorrow', 'Dec 25', or a weekday name. Pass all_day true for whole-day events, repeat_yearly true for birthdays/anniversaries. `message` is REQUIRED: the words the avatar speaks when it fires.".into(),
            input_schema: schema(
                json!({
                    "date": { "type": "string", "description": "Event date: YYYY-MM-DD, 'today', 'tomorrow', 'Dec 25', or weekday name" },
                    "hour": { "type": "integer", "description": "Hour in 24h local time, 0-23" },
                    "minute": { "type": "integer", "description": "Minute, 0-59" },
                    "all_day": { "type": "boolean", "description": "Whole-day event (fires 9am). Default false." },
                    "end": { "type": "string", "description": "Optional end time 'HH:MM' for display" },
                    "repeat_yearly": { "type": "boolean", "description": "Repeat every year (birthdays). Default false." },
                    "title": { "type": "string", "description": "Short name, e.g. 'Dentist appointment'" },
                    "description": { "type": "string", "description": "Extra details" },
                    "message": { "type": "string", "description": "REQUIRED. What the avatar says out loud when it fires." },
                }),
                &["message"],
            ),
        },
        ToolDef {
            name: "calendar_update".into(),
            description: "Change an existing calendar event by id. Call calendar_list first to find the id. Pass ONLY the fields to change; omitted fields keep their current value.".into(),
            input_schema: schema(
                json!({
                    "id": { "type": "string", "description": "Event id from calendar_list" },
                    "date": { "type": "string", "description": "New date" },
                    "hour": { "type": "integer", "description": "Hour in 24h local time, 0-23" },
                    "minute": { "type": "integer", "description": "Minute, 0-59" },
                    "all_day": { "type": "boolean", "description": "New all-day flag" },
                    "end": { "type": "string", "description": "New end time 'HH:MM', or empty to clear" },
                    "repeat_yearly": { "type": "boolean", "description": "New yearly-repeat flag" },
                    "title": { "type": "string", "description": "New title" },
                    "description": { "type": "string", "description": "New details" },
                    "message": { "type": "string", "description": "New spoken message" },
                    "enabled": { "type": "boolean", "description": "true to resume, false to pause" },
                }),
                &["id"],
            ),
        },
        ToolDef {
            name: "calendar_delete".into(),
            description: "Delete a calendar event by id. Call calendar_list first to find the id.".into(),
            input_schema: schema(
                json!({
                    "id": { "type": "string", "description": "Event id from calendar_list" },
                }),
                &["id"],
            ),
        },
    ]
}

// ---------------------------------------------------------------------------
// Plugin impl
// ---------------------------------------------------------------------------

/// Concrete plugin type handed to `export_plugin!`.
pub struct CalendarPlugin {
    inner: Option<Calendar>,
    ctx: Option<PluginCtx>,
}

impl Default for CalendarPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl CalendarPlugin {
    fn new() -> Self {
        CalendarPlugin {
            inner: None,
            ctx: None,
        }
    }
}

impl Plugin for CalendarPlugin {
    fn init(&mut self, ctx: PluginCtx) -> Result<(), String> {
        let data_dir = ctx.data_dir().ok_or("host did not provide a data dir")?;
        std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
        let mut calendar = Calendar {
            events: Arc::new(Mutex::new(Vec::new())),
            data_dir,
            stop: Arc::new(AtomicBool::new(false)),
        };
        calendar.load();
        spawn_scheduler(
            ctx.clone(),
            Arc::clone(&calendar.events),
            calendar.data_dir.clone(),
            Arc::clone(&calendar.stop),
        );
        ctx.info("calendar ready");
        self.ctx = Some(ctx);
        self.inner = Some(calendar);
        Ok(())
    }

    fn handle(&mut self, method: &str, payload: Value) -> Result<Value, String> {
        let (calendar, ctx) = match (self.inner.as_ref(), self.ctx.as_ref()) {
            (Some(c), Some(k)) => (c, k),
            _ => return Err("calendar plugin failed to initialise".into()),
        };
        calendar.route(ctx, method, payload)
    }

    fn tools(&mut self) -> Vec<ToolDef> {
        tool_defs()
    }

    fn shutdown(&mut self) {
        if let Some(calendar) = self.inner.as_ref() {
            calendar.stop.store(true, Ordering::Relaxed);
        }
    }
}

export_plugin!(CalendarPlugin::new());

#[cfg(test)]
mod tests {
    use super::*;

    fn calendar() -> Calendar {
        let dir = std::env::temp_dir().join(format!(
            "calendar-plugin-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Calendar {
            events: Arc::new(Mutex::new(Vec::new())),
            data_dir: dir,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    fn event(date: &str, hour: u8, minute: u8) -> CalendarEvent {
        CalendarEvent {
            id: "e".into(),
            title: "Dentist".into(),
            description: String::new(),
            date: date.into(),
            hour,
            minute,
            all_day: false,
            end_hour: None,
            end_minute: None,
            repeat: None,
            repeat_yearly: false,
            message: String::new(),
            enabled: true,
            last_fired: None,
        }
    }

    #[test]
    fn date_parser_covers_model_forms() {
        let today = Local::now().date_naive();
        assert_eq!(
            parse_date(&today.format("%Y-%m-%d").to_string()),
            Some(today)
        );
        assert_eq!(parse_date("today"), Some(today));
        assert_eq!(
            parse_date("tomorrow"),
            Some(today + chrono::Duration::days(1))
        );
        // Month-day rolls forward when already passed this year.
        let jan1 = parse_date("Jan 1").unwrap();
        assert_eq!((jan1.month(), jan1.day()), (1, 1));
        let dec25 = parse_date("25 Dec").unwrap();
        assert_eq!((dec25.month(), dec25.day()), (12, 25));
        assert!(parse_date("not a date").is_none());
        assert!(parse_date("").is_none());
    }

    #[test]
    fn fires_at_matching_minute_only() {
        let now = Local.with_ymd_and_hms(2026, 10, 16, 9, 30, 0).unwrap();
        let stamp = now.timestamp() / 60;
        let due = event("2026-10-16", 9, 30);
        assert!(is_due(&due, now, stamp));
        let wrong_min = event("2026-10-16", 9, 31);
        assert!(!is_due(&wrong_min, now, stamp));
        let wrong_day = event("2026-10-17", 9, 30);
        assert!(!is_due(&wrong_day, now, stamp));
    }

    #[test]
    fn yearly_matches_month_day_any_year() {
        let now = Local.with_ymd_and_hms(2026, 10, 16, 9, 30, 0).unwrap();
        let stamp = now.timestamp() / 60;
        let mut yearly = event("2020-10-16", 9, 30);
        yearly.repeat_yearly = true;
        assert!(is_due(&yearly, now, stamp));
        let mut other = event("2020-10-17", 9, 30);
        other.repeat_yearly = true;
        assert!(!is_due(&other, now, stamp));
    }

    #[test]
    fn all_day_fires_at_nine() {
        let nine = Local.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap();
        let stamp = nine.timestamp() / 60;
        let mut e = event("2026-10-16", 0, 0);
        e.all_day = true;
        assert!(is_due(&e, nine, stamp));
        let eight = Local.with_ymd_and_hms(2026, 10, 16, 8, 59, 0).unwrap();
        assert!(!is_due(&e, eight, eight.timestamp() / 60));
    }

    #[test]
    fn rejects_past_one_shot_dates() {
        let mut e = event("2000-01-01", 9, 0);
        assert!(sanitize(&mut e).is_err());
        let mut yearly = event("2000-01-01", 9, 0);
        yearly.repeat_yearly = true;
        assert!(sanitize(&mut yearly).is_ok());
        // Any recurring rule may start in the past.
        let mut weekly = event("2000-01-01", 9, 0);
        weekly.repeat = Some(Repeat::Weekly);
        assert!(sanitize(&mut weekly).is_ok());
        assert_eq!(weekly.repeat_yearly, false);
        assert_eq!(weekly.repeat, Some(Repeat::Weekly));
    }

    #[test]
    fn older_files_load_with_defaults() {
        let json = r#"{"id":"e","date":"2026-10-16","hour":9,"minute":30}"#;
        let e: CalendarEvent = serde_json::from_str(json).unwrap();
        assert!(e.enabled);
        assert!(!e.all_day);
        assert!(!e.repeat_yearly);
        assert_eq!(e.repeat_rule(), Repeat::None);
    }

    #[test]
    fn legacy_yearly_bool_maps_to_yearly_rule() {
        let json = r#"{"id":"e","date":"2020-10-16","hour":9,"minute":30,"repeat_yearly":true}"#;
        let e: CalendarEvent = serde_json::from_str(json).unwrap();
        assert_eq!(e.repeat_rule(), Repeat::Yearly);
    }

    #[test]
    fn sanitize_normalizes_repeat_fields() {
        let mut e = event("2030-03-10", 9, 0);
        e.repeat = Some(Repeat::Weekly);
        sanitize(&mut e).unwrap();
        assert_eq!(e.repeat, Some(Repeat::Weekly));
        assert!(!e.repeat_yearly);

        let mut b = event("2030-03-10", 9, 0);
        b.repeat = Some(Repeat::Yearly);
        sanitize(&mut b).unwrap();
        assert!(b.repeat_yearly);
    }

    #[test]
    fn occurs_on_matches_each_rule() {
        // 2026-10-16 is a Friday.
        let start = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        let sat = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let sun = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
        let mon = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let fri = NaiveDate::from_ymd_opt(2026, 10, 23).unwrap();

        let none = event("2026-10-16", 9, 0);
        assert!(occurs_on(&none, start));
        assert!(!occurs_on(&none, fri));

        let mut daily = event("2026-10-16", 9, 0);
        daily.repeat = Some(Repeat::Daily);
        assert!(occurs_on(&daily, sat));
        assert!(!occurs_on(
            &daily,
            NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
        ));

        let mut weekdays = event("2026-10-16", 9, 0);
        weekdays.repeat = Some(Repeat::Weekdays);
        assert!(occurs_on(&weekdays, fri));
        assert!(occurs_on(&weekdays, mon));
        assert!(!occurs_on(&weekdays, sat));
        assert!(!occurs_on(&weekdays, sun));

        let mut weekly = event("2026-10-16", 9, 0);
        weekly.repeat = Some(Repeat::Weekly);
        assert!(occurs_on(&weekly, fri));
        assert!(!occurs_on(&weekly, sat));
        assert!(!occurs_on(
            &weekly,
            NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()
        ));

        let mut monthly = event("2026-10-16", 9, 0);
        monthly.repeat = Some(Repeat::Monthly);
        assert!(!occurs_on(&monthly, sat));
        assert!(occurs_on(
            &monthly,
            NaiveDate::from_ymd_opt(2026, 11, 16).unwrap()
        ));
        // Months without the day skip silently.
        assert!(!occurs_on(
            &monthly,
            NaiveDate::from_ymd_opt(2026, 11, 30).unwrap()
        ));

        let mut yearly = event("2026-02-28", 9, 0);
        yearly.repeat = Some(Repeat::Yearly);
        assert!(occurs_on(
            &yearly,
            NaiveDate::from_ymd_opt(2031, 2, 28).unwrap()
        ));
        assert!(!occurs_on(
            &yearly,
            NaiveDate::from_ymd_opt(2031, 3, 1).unwrap()
        ));
    }

    #[test]
    fn recurring_before_start_never_fires() {
        let now = Local.with_ymd_and_hms(2026, 10, 15, 9, 30, 0).unwrap();
        let stamp = now.timestamp() / 60;
        let mut weekly = event("2026-10-16", 9, 30); // Thursday start
        weekly.repeat = Some(Repeat::Daily);
        assert!(!is_due(&weekly, now, stamp));
    }

    #[test]
    fn save_and_tool_roundtrip() {
        let c = calendar();
        let list = c.op_save(event("2030-03-10", 9, 0)).unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].id.is_empty());
        let out = c.tool_list().unwrap();
        assert!(out.contains("Dentist"), "{out}");
    }

    #[test]
    fn tool_create_parses_dates_and_times() {
        let c = calendar();
        let out = c
            .tool_create(&serde_json::json!({
                "date": "tomorrow", "hour": 15, "minute": 0,
                "title": "Dentist", "message": "dentist time"
            }))
            .unwrap();
        assert!(out.starts_with("Created event 'Dentist'"), "{out}");
        let list = c.events.lock().unwrap();
        assert_eq!(list[0].hour, 15);
    }
}
