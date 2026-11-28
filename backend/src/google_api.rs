//! Google Calendar API v3 client (blocking reqwest).
//!
//! Token lifecycles are owned by `google_sync.rs` — every call here takes an
//! explicit access token and reports `ApiError::Unauthorized` so the engine
//! can refresh once and retry.

use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

const BASE: &str = "https://www.googleapis.com/calendar/v3";
const MAX_RESULTS: u32 = 250;

pub enum ApiError {
    /// 401 — access token rejected; refresh and retry.
    Unauthorized(String),
    /// 404/410 — the calendar/event vanished; treat as needing a resync.
    Gone(String),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized(e) => write!(f, "unauthorized: {e}"),
            ApiError::Gone(e) => write!(f, "gone: {e}"),
            ApiError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// A calendar from `calendarList.list`. `primary == true` marks the user's
/// main calendar; its `id` is the account email address.
#[derive(Debug, Clone)]
pub struct GCalendar {
    pub id: String,
    pub summary: String,
    pub primary: bool,
    pub access_role: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GTime {
    /// All-day form: "YYYY-MM-DD".
    #[serde(default)]
    pub date: Option<String>,
    /// Timed form: RFC 3339 with offset ("2026-11-28T15:00:00-05:00").
    #[serde(default, rename = "dateTime")]
    pub date_time: Option<String>,
    /// IANA name ("America/New_York") for floating dateTimes.
    #[serde(default, rename = "timeZone")]
    pub time_zone: Option<String>,
}

/// One Google event. Only the fields the sync engine consumes are parsed.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct GEvent {
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub start: Option<GTime>,
    #[serde(default)]
    pub end: Option<GTime>,
    /// RFC 5545 lines ("RRULE:FREQ=DAILY", "EXDATE:…") when recurring.
    #[serde(default)]
    pub recurrence: Option<Vec<String>>,
}

/// One page of an incremental/full events.list response.
pub struct EventsPage {
    pub events: Vec<GEvent>,
    /// Persist and pass back on the next pull for incremental sync.
    pub next_sync_token: Option<String>,
    pub next_page_token: Option<String>,
}

pub struct GoogleApi {
    http: reqwest::blocking::Client,
}

impl GoogleApi {
    pub fn new() -> Result<GoogleApi, String> {
        Ok(GoogleApi {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| format!("http client: {e}"))?,
        })
    }

    fn check_status(status: u16, body: &str) -> ApiError {
        match status {
            401 => ApiError::Unauthorized(body.chars().take(200).collect()),
            404 | 410 => ApiError::Gone(body.chars().take(200).collect()),
            _ => ApiError::Other(format!("Google API {status}: {}", body.chars().take(300).collect::<String>())),
        }
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<String, ApiError> {
        let resp = req.send().map_err(|e| ApiError::Other(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().unwrap_or_default();
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(Self::check_status(status, &body))
        }
    }

    fn get_json(&self, url: &str, token: &str) -> Result<Value, ApiError> {
        let body = self.send(self.http.get(url).header("Authorization", format!("Bearer {token}")))?;
        serde_json::from_str(&body)
            .map_err(|e| ApiError::Other(format!("bad Google API response: {e}")))
    }

    /// GET /users/me/calendarList
    pub fn calendar_list(&self, token: &str) -> Result<Vec<GCalendar>, ApiError> {
        let url = format!("{BASE}/users/me/calendarList?maxResults=250&showDeleted=false&showHidden=false");
        let json = self.get_json(&url, token)?;
        let mut out = Vec::new();
        if let Some(items) = json.get("items").and_then(Value::as_array) {
            for item in items {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                // Calendars the user can only view (holidays, subscribed)
                // cannot accept pushes but still sync into the app.
                out.push(GCalendar {
                    id: id.to_string(),
                    summary: item
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or("(untitled)")
                        .to_string(),
                    primary: item.get("primary").and_then(Value::as_bool).unwrap_or(false),
                    access_role: item
                        .get("accessRole")
                        .and_then(Value::as_str)
                        .unwrap_or("reader")
                        .to_string(),
                });
            }
        }
        Ok(out)
    }

    /// GET /calendars/{id}/events — one page (250 events).
    ///
    /// Two modes, mutually exclusive per Google's contract:
    /// - incremental: `sync_token` set, no time window;
    /// - full: `time_min`/`time_max` set (RFC 3339), no sync token.
    pub fn events(
        &self,
        token: &str,
        calendar_id: &str,
        sync_token: Option<&str>,
        time_min: Option<&str>,
        time_max: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<EventsPage, ApiError> {
        let mut url = format!(
            "{BASE}/calendars/{}/events?maxResults={MAX_RESULTS}&showDeleted=true&singleEvents=false",
            encode_path(calendar_id)
        );
        if let Some(t) = sync_token {
            url.push_str(&format!("&syncToken={}", urlencode(t)));
        }
        if let Some(t) = time_min {
            url.push_str(&format!("&timeMin={}", urlencode(t)));
        }
        if let Some(t) = time_max {
            url.push_str(&format!("&timeMax={}", urlencode(t)));
        }
        if let Some(t) = page_token {
            url.push_str(&format!("&pageToken={}", urlencode(t)));
        }
        let json = self.get_json(&url, token)?;
        let mut events = Vec::new();
        if let Some(items) = json.get("items").and_then(Value::as_array) {
            for item in items {
                match serde_json::from_value::<GEvent>(item.clone()) {
                    Ok(ev) => events.push(ev),
                    Err(_) => continue,
                }
            }
        }
        Ok(EventsPage {
            events,
            next_sync_token: json
                .get("nextSyncToken")
                .and_then(Value::as_str)
                .map(str::to_string),
            next_page_token: json
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    /// POST /calendars/{id}/events — returns the created event (with its id).
    pub fn insert_event(&self, token: &str, calendar_id: &str, body: &Value) -> Result<GEvent, ApiError> {
        let url = format!("{BASE}/calendars/{}/events", encode_path(calendar_id));
        let resp = self.send(
            self.http
                .post(&url)
                .header("Authorization", format!("Bearer {token}"))
                .json(body),
        )?;
        serde_json::from_str(&resp).map_err(|e| ApiError::Other(format!("bad insert response: {e}")))
    }

    /// PATCH /calendars/{id}/events/{eventId}
    pub fn patch_event(
        &self,
        token: &str,
        calendar_id: &str,
        google_event_id: &str,
        body: &Value,
    ) -> Result<GEvent, ApiError> {
        let url = format!(
            "{BASE}/calendars/{}/events/{}",
            encode_path(calendar_id),
            encode_path(google_event_id)
        );
        let resp = self.send(
            self.http
                .patch(&url)
                .header("Authorization", format!("Bearer {token}"))
                .json(body),
        )?;
        serde_json::from_str(&resp).map_err(|e| ApiError::Other(format!("bad patch response: {e}")))
    }

    /// DELETE /calendars/{id}/events/{eventId} — already-gone is success.
    pub fn delete_event(&self, token: &str, calendar_id: &str, google_event_id: &str) -> Result<(), ApiError> {
        let url = format!(
            "{BASE}/calendars/{}/events/{}",
            encode_path(calendar_id),
            encode_path(google_event_id)
        );
        match self.send(
            self.http
                .delete(&url)
                .header("Authorization", format!("Bearer {token}")),
        ) {
            Ok(_) => Ok(()),
            Err(ApiError::Gone(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Calendar ids are opaque strings ("user@gmail.com", "abc#group.calendar.google.com",
/// base64-ish ids for shared calendars); event ids likewise. Percent-encode
/// every path segment conservatively.
fn encode_path(segment: &str) -> String {
    urlencode(segment)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_google_event_shapes() {
        let json = serde_json::json!({
            "id": "evt1",
            "status": "confirmed",
            "summary": "Lunch",
            "description": "with team",
            "start": { "dateTime": "2026-11-28T12:00:00-05:00" },
            "end": { "dateTime": "2026-11-28T13:00:00-05:00" }
        });
        let ev: GEvent = serde_json::from_value(json).unwrap();
        assert_eq!(ev.id, "evt1");
        assert_eq!(
            ev.start.as_ref().and_then(|s| s.date_time.as_deref()),
            Some("2026-11-28T12:00:00-05:00")
        );
        assert!(ev.recurrence.is_none());

        let all_day: GEvent = serde_json::from_value(serde_json::json!({
            "id": "evt2",
            "start": { "date": "2026-12-25" },
            "end": { "date": "2026-12-26" },
            "recurrence": ["RRULE:FREQ=YEARLY"]
        }))
        .unwrap();
        assert_eq!(all_day.start.unwrap().date.as_deref(), Some("2026-12-25"));
        assert_eq!(
            all_day.recurrence,
            Some(vec!["RRULE:FREQ=YEARLY".to_string()])
        );
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(encode_path("user@gmail.com"), "user%40gmail.com");
        assert_eq!(encode_path("abc#group.calendar.google.com"), "abc%23group.calendar.google.com");
        assert_eq!(encode_path("plain-id_1.2"), "plain-id_1.2");
    }
}
