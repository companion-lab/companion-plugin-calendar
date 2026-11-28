// Google Calendar sync card for the Calendar settings section.
//
// States: disconnected (client-id entry) → awaiting_callback (browser open)
// → connected (calendar picker, sync interval, Sync now / Disconnect).
// Status flows from the backend's `google.status` method, refreshed on the
// `google-status-changed` event the sync worker emits.

import { useCallback, useEffect, useMemo, useState } from "react";
import type { PluginContext } from "@companion-lab/plugin";
import { useGoogleStatus, type GoogleCalendarSummary } from "./backend";
import { tauriInvoke } from "./tauri";
import { ConfirmModal } from "./ConfirmModal";

const INTERVAL_CHOICES = [5, 15, 30, 60, 240];

function labelForInterval(min: number): string {
  if (min < 60) return `Every ${min} min`;
  if (min === 60) return "Hourly";
  return `Every ${min / 60} h`;
}

function relativeSync(ts: number | null): string {
  if (!ts) return "never";
  const diff = Date.now() - ts;
  if (diff < 90_000) return "just now";
  if (diff < 3_600_000) return `${Math.round(diff / 60_000)} min ago`;
  if (diff < 86_400_000) return `${Math.round(diff / 3_600_000)} h ago`;
  return new Date(ts).toLocaleDateString();
}

function GoogleIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" aria-hidden style={{ flexShrink: 0 }}>
      <path fill="#4285F4" d="M23.5 12.3c0-.9-.1-1.5-.3-2.2H12v4.1h6.5c-.1 1.1-.8 2.7-2.4 3.8l3.7 2.9c2.2-2 3.7-5 3.7-8.6z" />
      <path fill="#34A853" d="M12 24c3.2 0 5.9-1.1 7.9-2.9l-3.7-2.9c-1 .7-2.4 1.2-4.2 1.2-3.2 0-5.9-2.1-6.9-5l-3.9 3C3.3 21.3 7.3 24 12 24z" />
      <path fill="#FBBC05" d="M5.1 14.4c-.3-.7-.4-1.5-.4-2.4s.2-1.7.4-2.4l-3.9-3C.4 8.2 0 10 0 12s.4 3.8 1.2 5.4l3.9-3z" />
      <path fill="#EA4335" d="M12 4.7c1.8 0 3 .8 3.7 1.4l3.3-3.2C17.9 1.1 15.2 0 12 0 7.3 0 3.3 2.7 1.2 6.6l3.9 3c1-2.9 3.7-4.9 6.9-4.9z" />
    </svg>
  );
}

export function GoogleSyncCard({ ctx }: { ctx: PluginContext }) {
  const { ui } = ctx;
  const { Button, TextInput, Select, FormField } = ui;
  const { status, refresh, api } = useGoogleStatus(ctx);
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [connecting, setConnecting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [calendars, setCalendars] = useState<GoogleCalendarSummary[] | null>(null);
  const [calendarsError, setCalendarsError] = useState<string | null>(null);
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);
  const [syncingNow, setSyncingNow] = useState(false);

  useEffect(() => {
    if (status?.connected) setShowSettings(true);
  }, [status?.connected]);

  const loadCalendars = useCallback(async () => {
    setCalendarsError(null);
    try {
      setCalendars(await api.calendars());
    } catch (e) {
      setCalendarsError(e instanceof Error ? e.message : String(e));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (status?.connected && calendars == null) void loadCalendars();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status?.connected]);

  const connect = async () => {
    setError(null);
    setConnecting(true);
    try {
      const { auth_url } = await api.connect(clientId.trim(), clientSecret);
      // The consent screen must open in the real browser (Google's loopback
      // redirect lands on the backend's local callback server).
      await tauriInvoke("open_url", { url: auth_url });
      setClientSecret("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setConnecting(false);
  };

  const disconnect = async () => {
    setConfirmDisconnect(false);
    try {
      await api.disconnect();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setCalendars(null);
  };

  const syncNow = async () => {
    setSyncingNow(true);
    try {
      await api.syncNow();
      // The status event refreshes the card when the cycle ends.
      setTimeout(() => setSyncingNow(false), 4000);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSyncingNow(false);
    }
  };

  const saveConfig = async (patch: {
    selected_calendars?: string[];
    default_calendar?: string;
    interval_minutes?: number;
  }) => {
    setError(null);
    try {
      await api.saveConfig(patch);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const toggleCalendar = (id: string, on: boolean) => {
    if (!status) return;
    const current = status.selected_calendars;
    const next = on ? [...current, id] : current.filter((c) => c !== id);
    // The push target must stay among the selected calendars.
    const patch: { selected_calendars: string[]; default_calendar?: string } = {
      selected_calendars: next,
    };
    if (!on && status.default_calendar === id) {
      patch.default_calendar = next.find((c) => c === status.account) ?? next[0] ?? "";
    }
    void saveConfig(patch);
  };

  const selectedSet = useMemo(
    () => new Set(status?.selected_calendars ?? []),
    [status?.selected_calendars],
  );

  if (!status?.available) return null;

  const phase = status.phase;
  const connected = status.connected;

  return (
    <div
      className="cal-gcard"
      style={{
        border: "1px solid var(--border-subtle)",
        borderRadius: "var(--radius-md)",
        padding: 14,
        marginBottom: 14,
        display: "flex",
        flexDirection: "column",
        gap: 10,
        // Allow shrinking below the content's min width inside flex parents,
        // so long unbreakable strings (calendar ids) can't widen the card.
        minWidth: 0,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <GoogleIcon />
        <strong style={{ fontSize: 13 }}>Google Calendar</strong>
        <span className="item-meta">
          {connected
            ? status.syncing
              ? "syncing…"
              : `synced ${relativeSync(status.last_sync)}`
            : phase === "awaiting_callback"
              ? "waiting for Google…"
              : "not connected"}
        </span>
        <span style={{ flex: 1 }} />
        {connected && (
          <>
            {showSettings && (
              <Button
                variant="primary"
                onClick={syncNow}
                disabled={syncingNow || status.syncing}
                style={{ padding: "4px 10px", fontSize: 11 }}
              >
                {syncingNow || status.syncing ? "Syncing…" : "Sync now"}
              </Button>
            )}
            <Button
              variant="secondary"
              onClick={() => setShowSettings((v) => !v)}
              style={{ padding: "4px 10px", fontSize: 11 }}
            >
              {showSettings ? "Hide" : "Settings"}
            </Button>
          </>
        )}
      </div>

      {connected && status.account && (
        <div className="item-meta">Connected as {status.account}</div>
      )}
      {phase === "needs_login" && status.error && (
        <div className="error-banner">Google session expired: {status.error}</div>
      )}
      {!connected && phase !== "awaiting_callback" && status.error && (
        <div className="error-banner">{status.error}</div>
      )}
      {error && <div className="error-banner">{error}</div>}

      {phase === "awaiting_callback" && (
        <div className="item-meta">
          Finish signing in with Google in your browser, then this panel updates automatically.
        </div>
      )}

      {!connected && phase !== "awaiting_callback" && (
        <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
          <div className="item-meta">
            Two-way sync between this calendar and your Google calendars. One-time setup: in{" "}
            <a
              href="https://console.cloud.google.com/apis/credentials"
              onClick={(e) => {
                e.preventDefault();
                void tauriInvoke("open_url", {
                  url: "https://console.cloud.google.com/apis/credentials",
                }).catch(() => {});
              }}
              style={{ color: "var(--accent)" }}
            >
              Google Cloud Console
            </a>
            , enable the <b>Google Calendar API</b> and create an OAuth client ID of type{" "}
            <b>Desktop app</b>; paste its values below. Events you add in the app are pushed to
            your default Google calendar, and Google changes are pulled every few minutes.
          </div>
          <FormField label="Client ID">
            <TextInput
              value={clientId}
              placeholder="1234567890-abc.apps.googleusercontent.com"
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => setClientId(e.target.value)}
            />
          </FormField>
          <FormField label="Client secret">
            <TextInput
              value={clientSecret}
              placeholder="GOCSPX-…"
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => setClientSecret(e.target.value)}
            />
          </FormField>
          <Button
            variant="primary"
            onClick={connect}
            disabled={connecting || !clientId.trim() || !clientSecret.trim()}
          >
            {connecting ? "Opening Google…" : "Connect Google account"}
          </Button>
        </div>
      )}

      {connected && showSettings && (
        <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
          <div className="st-field">
            <span className="st-field-label">Calendars</span>
            {calendarsError && <div className="error-banner">{calendarsError}</div>}
            {calendars == null && !calendarsError && (
              <div className="item-meta">Loading calendars…</div>
            )}
            {calendars != null && (
              <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 6 }}>
                {calendars.map((c) => (
                  <label
                    key={c.id}
                    style={{ display: "flex", flexDirection: "column", gap: 1, cursor: "pointer", minWidth: 0 }}
                  >
                    <span style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <input
                        type="checkbox"
                        checked={selectedSet.has(c.id)}
                        onChange={(e) => toggleCalendar(c.id, e.target.checked)}
                      />
                      <span style={{ fontSize: 13 }}>{c.summary}</span>
                      {c.primary && <span className="item-meta">primary</span>}
                    </span>
                    {/* Calendar ids are long unbreakable emails; keep them on
                        their own wrapping line so they can't widen the card. */}
                    <span className="item-meta" style={{ paddingLeft: 22, wordBreak: "break-all" }}>
                      {c.id}
                    </span>
                  </label>
                ))}
                {calendars.length === 0 && (
                  <div className="item-meta">No calendars found.</div>
                )}
              </div>
            )}
            <span className="st-field-hint">
              Selected calendars are pulled into this calendar. Events sync both ways —
              edit them here and changes push back to Google. Complex Google repeats
              simplify to the app's repeat options.
            </span>
          </div>

          <div style={{ display: "flex", gap: 10 }}>
            <FormField label="Push new events to">
              <Select
                aria-label="Default calendar"
                value={status.default_calendar ?? ""}
                onChange={(e: React.ChangeEvent<HTMLSelectElement>) =>
                  void saveConfig({ default_calendar: e.target.value })
                }
              >
                <option value="">(choose a calendar)</option>
                {(calendars ?? [])
                  .filter((c) => selectedSet.has(c.id))
                  .map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.summary}
                    </option>
                  ))}
              </Select>
            </FormField>
            <FormField label="Sync interval">
              <Select
                aria-label="Sync interval"
                value={status.interval_minutes}
                onChange={(e: React.ChangeEvent<HTMLSelectElement>) =>
                  void saveConfig({ interval_minutes: Number(e.target.value) })
                }
              >
                {INTERVAL_CHOICES.map((m) => (
                  <option key={m} value={m}>
                    {labelForInterval(m)}
                  </option>
                ))}
              </Select>
            </FormField>
          </div>
          {status.queue_len > 0 && (
            <div className="item-meta">{status.queue_len} change(s) waiting to be pushed.</div>
          )}
          <div>
            <Button variant="danger" onClick={() => setConfirmDisconnect(true)} style={{ padding: "5px 10px", fontSize: 11 }}>
              Disconnect
            </Button>
          </div>
        </div>
      )}

      {confirmDisconnect && (
        <ConfirmModal
          ui={ui}
          title="Disconnect Google Calendar?"
          message="Google events already synced stay in this calendar but stop updating, and local changes are no longer pushed. You can reconnect at any time."
          onClose={() => setConfirmDisconnect(false)}
          onConfirm={disconnect}
        />
      )}
    </div>
  );
}
