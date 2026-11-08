// TimePicker (ported from the host's organisms/AlarmsSection.tsx; the alarms
// plugin carries the same copy). UI kit comes in via the `ui` prop so this
// file stays self-contained inside the plugin.

import { useEffect, useId, useRef, useState } from "react";
import type { PluginContext } from "@companion-lab/plugin";

type Ampm = "AM" | "PM";

function to12h(hour24: number): { h12: number; ampm: Ampm } {
  return { h12: hour24 % 12 === 0 ? 12 : hour24 % 12, ampm: hour24 < 12 ? "AM" : "PM" };
}

function to24h(h12: number, ampm: Ampm): number {
  return (Math.min(12, Math.max(1, h12)) % 12) + (ampm === "PM" ? 12 : 0);
}

function format12h(hour: number, minute: number): string {
  const { h12, ampm } = to12h(hour);
  return `${String(h12).padStart(2, "0")}:${String(minute).padStart(2, "0")} ${ampm}`;
}

function parseTime(raw: string): { hour: number; minute: number } | null {
  let s = raw.trim().toLowerCase().replace(/\./g, "").replace(/\s+/g, " ");
  let ampm: Ampm | null = null;
  const ap = s.match(/\s*([ap])(m?)\s*$/);
  if (ap) {
    ampm = ap[1] === "a" ? "AM" : "PM";
    s = s.slice(0, ap.index).trim();
  }
  const m = s.match(/^(\d{1,2})(?::(\d{1,2}))?$/);
  if (!m) return null;
  const h = Number(m[1]);
  const min = m[2] === undefined ? 0 : Number(m[2]);
  if (min > 59) return null;
  if (ampm) {
    if (h < 1 || h > 12) return null;
    return { hour: to24h(h, ampm), minute: min };
  }
  if (h > 23) return null;
  return { hour: h, minute: min };
}

export function TimePicker({ ui, hour, minute, onChange, onClear, placeholder }: {
  ui: PluginContext["ui"];
  hour?: number | null;
  minute?: number | null;
  onChange: (hour: number, minute: number) => void;
  onClear?: () => void;
  placeholder?: string;
}) {
  const hasValue = hour != null && minute != null;
  const safeHour = hour ?? 12;
  const safeMinute = minute ?? 0;
  const [open, setOpen] = useState(false);
  const [text, setText] = useState(() => (hasValue ? format12h(safeHour, safeMinute) : ""));
  const [hourText, setHourText] = useState(() => String(to12h(safeHour).h12));
  const [minuteText, setMinuteText] = useState(() => String(safeMinute).padStart(2, "0"));
  const [bad, setBad] = useState(false);
  const [hourFocused, setHourFocused] = useState(false);
  const [minuteFocused, setMinuteFocused] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  const uid = useId().replace(/:/g, "");
  const hourListId = `tp-hour-${uid}`;
  const minuteListId = `tp-minute-${uid}`;
  const { h12, ampm } = to12h(safeHour);

  useEffect(() => {
    if (hour != null && minute != null) {
      setText(format12h(hour, minute));
      setBad(false);
      if (!hourFocused) setHourText(String(to12h(hour).h12));
      if (!minuteFocused) setMinuteText(String(minute).padStart(2, "0"));
    } else {
      setText("");
      setBad(false);
    }
  }, [hour, minute, hourFocused, minuteFocused]);

  useEffect(() => {
    if (!open) return;
    setHourText(String(to12h(hour ?? 12).h12));
    setMinuteText(String(minute ?? 0).padStart(2, "0"));
    const onDown = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, hour, minute]);

  const commit = () => {
    if (!text.trim()) {
      if (onClear) {
        setBad(false);
        onClear();
        return;
      }
    }
    const p = parseTime(text);
    if (p) {
      setBad(false);
      if (p.hour !== hour || p.minute !== minute) onChange(p.hour, p.minute);
      else setText(format12h(p.hour, p.minute));
    } else {
      setBad(true);
      setText(hasValue ? format12h(safeHour, safeMinute) : "");
    }
  };

  const onHourBox = (raw: string) => {
    const digits = raw.replace(/\D/g, "").slice(0, 2);
    if (!digits) {
      setHourText("");
      return;
    }
    const h = Math.min(12, Number(digits));
    setHourText(String(h));
    if (h >= 1) onChange(to24h(h, ampm), safeMinute);
  };

  const onMinuteBox = (raw: string) => {
    const digits = raw.replace(/\D/g, "").slice(0, 2);
    setMinuteText(digits);
    if (!digits) return;
    const m = Math.min(59, Number(digits));
    if (m !== safeMinute) onChange(safeHour, m);
  };

  const commitHourBox = () => {
    const h = Number(hourText);
    setHourText(!hourText || !Number.isInteger(h) || h < 1 ? String(h12) : String(Math.min(12, h)));
  };

  const commitMinuteBox = () => {
    const m = Number(minuteText);
    setMinuteText(minuteText === "" || !Number.isInteger(m) || m < 0 ? String(safeMinute).padStart(2, "0") : String(Math.min(59, m)).padStart(2, "0"));
  };

  const useNow = () => {
    const n = new Date();
    onChange(n.getHours(), n.getMinutes());
  };

  const handleClear = () => {
    if (onClear) {
      onClear();
      setOpen(false);
    } else {
      onChange(0, 0);
    }
  };

  const { TextInput } = ui;
  return (
    <div ref={wrapRef} style={{ position: "relative" }}>
      <div style={{ position: "relative" }}>
        <TextInput
          aria-label="Time"
          placeholder={placeholder ?? "HH:MM"}
          value={text}
          onChange={(e: React.ChangeEvent<HTMLInputElement>) => setText(e.target.value)}
          onBlur={commit}
          onKeyDown={(e: React.KeyboardEvent) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          }}
          style={{ width: "100%", paddingRight: 34, ...(bad ? { borderColor: "var(--danger)" } : null) }}
        />
        <button
          type="button"
          aria-label="Pick time"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
          style={{
            position: "absolute",
            right: 4,
            top: "50%",
            transform: "translateY(-50%)",
            background: "none",
            border: "none",
            cursor: "pointer",
            opacity: 0.55,
            padding: 6,
            color: "inherit",
          }}
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
            <circle cx="12" cy="12" r="9" />
            <path d="M12 7v5l3 2" />
          </svg>
        </button>
      </div>
      {open && (
        <div
          style={{
            position: "absolute",
            top: "100%",
            left: 0,
            right: 0,
            zIndex: 20,
            marginTop: 4,
            padding: "16px 16px 10px",
            background: "var(--bg-input)",
            border: "1px solid var(--border-subtle)",
            borderRadius: "var(--radius-md)",
          }}
        >
          <div style={{ fontSize: 14, fontWeight: 600, marginBottom: 12, color: "var(--text-secondary)" }}>Enter time</div>
          <div style={{ display: "flex", gap: 8, alignItems: "stretch" }}>
            <TextInput
              aria-label="Hour"
              inputMode="numeric"
              list={hourListId}
              value={hourText}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => onHourBox(e.target.value)}
              onBlur={() => {
                setHourFocused(false);
                commitHourBox();
              }}
              onFocus={(e: React.FocusEvent<HTMLInputElement>) => {
                setHourFocused(true);
                e.target.select();
              }}
              style={{ flex: 1, minWidth: 0, fontSize: 40, textAlign: "center", padding: "10px 0", background: "rgba(127,127,127,0.15)", borderColor: "transparent" }}
            />
            <datalist id={hourListId}>
              {Array.from({ length: 12 }, (_, i) => i + 1).map((h) => (
                <option key={h} value={h} />
              ))}
            </datalist>
            <div aria-hidden="true" style={{ alignSelf: "center", fontSize: 30, fontWeight: 700, opacity: 0.7 }}>:</div>
            <TextInput
              aria-label="Minute"
              inputMode="numeric"
              list={minuteListId}
              value={minuteText}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) => onMinuteBox(e.target.value)}
              onBlur={() => {
                setMinuteFocused(false);
                commitMinuteBox();
              }}
              onFocus={(e: React.FocusEvent<HTMLInputElement>) => {
                setMinuteFocused(true);
                e.target.select();
              }}
              style={{ flex: 1, minWidth: 0, fontSize: 40, textAlign: "center", padding: "10px 0", background: "rgba(127,127,127,0.15)", borderColor: "transparent" }}
            />
            <datalist id={minuteListId}>
              {Array.from({ length: 12 }, (_, i) => i * 5).map((m) => (
                <option key={m} value={String(m).padStart(2, "0")} />
              ))}
            </datalist>
            <div style={{ display: "flex", flexDirection: "column", border: "1px solid var(--border-subtle)", borderRadius: 8, overflow: "hidden", width: 64, flexShrink: 0 }}>
              {(["AM", "PM"] as const).map((a, i) => (
                <button
                  key={a}
                  type="button"
                  onClick={() => onChange(to24h(h12, a), safeMinute)}
                  style={{
                    flex: 1,
                    padding: "8px 0",
                    fontSize: 13,
                    fontWeight: 600,
                    border: "none",
                    cursor: "pointer",
                    background: a === ampm ? "var(--accent-subtle)" : "transparent",
                    color: a === ampm ? "var(--accent-hover)" : "var(--text-muted)",
                    ...(i === 0 ? { borderBottom: "1px solid var(--border-subtle)" } : null),
                  }}
                >
                  {a}
                </button>
              ))}
            </div>
          </div>
          <div style={{ display: "flex", justifyContent: "space-between", borderTop: "1px solid var(--border-subtle)", marginTop: 12, paddingTop: 8 }}>
            <button
              type="button"
              onClick={useNow}
              style={{ background: "none", border: "none", cursor: "pointer", color: "var(--accent-hover)", fontSize: 13, fontWeight: 600, padding: "4px 8px" }}
            >
              Now
            </button>
            <button
              type="button"
              onClick={handleClear}
              style={{ background: "none", border: "none", cursor: "pointer", color: "var(--accent-hover)", fontSize: 13, fontWeight: 600, padding: "4px 8px" }}
            >
              Clear
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
