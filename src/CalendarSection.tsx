// Calendar settings section (ported from the host's organisms/CalendarSection.tsx;
// UI kit comes from the host via ctx.ui and storage from the plugin backend).

import { useEffect, useMemo, useRef, useState } from "react";
// react-big-calendar comes from the vendored, pre-bundled copy (see
// scripts/build-vendor.mjs): the plugin build aliases bare "react" imports to
// the SDK host shim, which predates react-big-calendar's needs (createRef,
// createPortal). Types still come from the installed package.
import { Calendar, dateFnsLocalizer } from "./vendor/react-big-calendar.mjs";
import type { View, ToolbarProps } from "react-big-calendar";
import { format, parse, startOfWeek, getDay } from "date-fns";
import { enUS } from "date-fns/locale";
import { injectStyles } from "./styles";
import type { PluginContext } from "../../../sdk/js/@companion/plugin";
import { useCalendar, type CalendarApi } from "./backend";
import { TimePicker } from "./TimePicker";
import { ServerFormModal } from "./ServerFormModal";
import { ConfirmModal } from "./ConfirmModal";
import {
  emptyEvent,
  eventLabel,
  eventStatus,
  eventOccurrencesInRange,
  dateLabel,
  parseDateInput,
  repeatLabel,
  repeatRule,
  sortEvents,
  toIsoDate,
  type CalendarEvent,
  type Repeat,
} from "./calendar";

injectStyles();

const REPEAT_RULES: Repeat[] = ["none", "daily", "weekdays", "weekly", "monthly", "yearly"];

const localizer = dateFnsLocalizer({
  format,
  parse,
  startOfWeek,
  getDay,
  locales: { "en-US": enUS },
});

function MonthYearToolbar(props: ToolbarProps) {
  const { date, view, label, onNavigate, onView } = props;
  const month = date.getMonth();
  const year = date.getFullYear();
  const thisYear = new Date().getFullYear();
  const years = Array.from({ length: 61 }, (_, i) => thisYear - 30 + i);
  if (!years.includes(year)) years.push(year);
  years.sort((a, b) => a - b);
  const months = Array.from({ length: 12 }, (_, i) => format(new Date(2000, i, 1), "MMMM"));
  const goto = (m: number, y: number) => onNavigate("DATE", new Date(y, m, 1));
  return (
    <div className="rbc-toolbar cal-toolbar">
      <span className="rbc-btn-group cal-nav">
        <button type="button" aria-label="Previous" onClick={() => onNavigate("PREV")}>‹</button>
        <button type="button" onClick={() => onNavigate("TODAY")}>Today</button>
        <button type="button" aria-label="Next" onClick={() => onNavigate("NEXT")}>›</button>
      </span>
      <span className="cal-toolbar-picks" role="group" aria-label={label}>
        <select aria-label="Month" value={month} onChange={(e) => goto(Number(e.target.value), year)}>
          {months.map((name, i) => (
            <option key={name} value={i}>{name}</option>
          ))}
        </select>
        <select aria-label="Year" value={year} onChange={(e) => goto(month, Number(e.target.value))}>
          {years.map((y) => (
            <option key={y} value={y}>{y}</option>
          ))}
        </select>
      </span>
      <span className="rbc-btn-group cal-views">
        {(["month", "agenda"] as View[]).map((v) => (
          <button
            key={v}
            type="button"
            className={v === view ? "rbc-active" : undefined}
            onClick={() => onView(v)}
          >
            {v === "month" ? "Month" : "Agenda"}
          </button>
        ))}
      </span>
    </div>
  );
}

function CalendarMonthView({ events, onEdit, onCreateDate }: {
  events: CalendarEvent[];
  onEdit: (e: CalendarEvent) => void;
  onCreateDate: (iso: string) => void;
}) {
  const [cursor, setCursor] = useState(() => new Date());
  const [view, setView] = useState<View>("month");
  const occurrences = useMemo(() => {
    if (view === "agenda") {
      const end = new Date(cursor);
      end.setDate(end.getDate() + 30);
      return eventOccurrencesInRange(events, cursor, end);
    }
    const start = new Date(cursor.getFullYear(), cursor.getMonth(), 1);
    start.setDate(start.getDate() - 7);
    const end = new Date(cursor.getFullYear(), cursor.getMonth() + 1, 0);
    end.setDate(end.getDate() + 7);
    return eventOccurrencesInRange(events, start, end);
  }, [events, cursor, view]);
  const bigEvents = useMemo(
    () =>
      occurrences.map((o, i) => ({
        id: `${o.event.id}-${i}`,
        title: o.event.all_day ? o.event.title : `${o.event.title} ${format(o.start, "h:mm a")}`,
        start: o.start,
        end: o.end,
        allDay: o.event.all_day,
        resource: o.event,
      })),
    [occurrences],
  );
  return (
    <div className="cal-month">
      <Calendar
        localizer={localizer}
        events={bigEvents}
        date={cursor}
        onNavigate={setCursor}
        view={view}
        onView={setView}
        views={["month", "agenda"]}
        defaultView="month"
        popup
        selectable
        components={{ toolbar: MonthYearToolbar }}
        onSelectSlot={(slot) => {
          const d = slot.start;
          onCreateDate(toIsoDate(d.getFullYear(), d.getMonth(), d.getDate()));
        }}
        onSelectEvent={(be) => onEdit((be.resource ?? be) as CalendarEvent)}
        eventPropGetter={(be) => ({
          className: (be.resource as CalendarEvent | undefined)?.enabled === false ? "cal-ev-dim" : undefined,
        })}
        style={{ height: 480 }}
      />
    </div>
  );
}

function DatePicker({ ui, value, onChange }: {
  ui: PluginContext["ui"];
  value: string;
  onChange: (iso: string) => void;
}) {
  const { TextInput } = ui;
  const [open, setOpen] = useState(false);
  const [text, setText] = useState(() => dateLabel(value));
  const [bad, setBad] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  const [view, setView] = useState(() => {
    const m = value.match(/^(\d{4})-(\d{2})-(\d{2})$/);
    return m ? new Date(Number(m[1]), Number(m[2]) - 1, 1) : new Date();
  });

  useEffect(() => {
    setText(dateLabel(value));
    setBad(false);
  }, [value]);

  useEffect(() => {
    if (!open) return;
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
  }, [open]);

  const commit = () => {
    const iso = parseDateInput(text);
    if (iso) {
      setBad(false);
      if (iso !== value) onChange(iso);
      else setText(dateLabel(value));
    } else {
      setBad(true);
      setText(dateLabel(value));
    }
  };

  const vy = view.getFullYear();
  const vm = view.getMonth();
  const firstDow = new Date(vy, vm, 1).getDay();
  const daysInMonth = new Date(vy, vm + 1, 0).getDate();
  const cells: (number | null)[] = [
    ...Array.from({ length: firstDow }, () => null),
    ...Array.from({ length: daysInMonth }, (_, i) => i + 1),
  ];
  while (cells.length % 7 !== 0) cells.push(null);
  const todayIso = toIsoDate(new Date().getFullYear(), new Date().getMonth(), new Date().getDate());
  const step = (d: number) => setView(new Date(vy, vm + d, 1));

  return (
    <div ref={wrapRef} style={{ position: "relative" }}>
      <div style={{ position: "relative" }}>
        <TextInput
          aria-label="Date"
          placeholder="Dec 25, 2026"
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
          aria-label="Pick date"
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
            <rect x="3" y="4" width="18" height="18" rx="2" ry="2" />
            <line x1="16" y1="2" x2="16" y2="6" />
            <line x1="8" y1="2" x2="8" y2="6" />
            <line x1="3" y1="10" x2="21" y2="10" />
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
            padding: "14px 14px 10px",
            background: "var(--bg-input)",
            border: "1px solid var(--border-subtle)",
            borderRadius: "var(--radius-md)",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 10 }}>
            <button type="button" aria-label="Previous month" onClick={() => step(-1)} style={{ background: "none", border: "none", cursor: "pointer", color: "var(--text-secondary)", fontSize: 16, padding: "2px 8px" }}>‹</button>
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              <select aria-label="Month" value={vm} onChange={(e) => setView(new Date(vy, Number(e.target.value), 1))} className="dp-pick">
                {["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].map((m, i) => (
                  <option key={m} value={i}>{m}</option>
                ))}
              </select>
              <select aria-label="Year" value={vy} onChange={(e) => setView(new Date(Number(e.target.value), vm, 1))} className="dp-pick">
                {Array.from({ length: 101 }, (_, i) => new Date().getFullYear() - 50 + i).map((y) => (
                  <option key={y} value={y}>{y}</option>
                ))}
              </select>
            </div>
            <button type="button" aria-label="Next month" onClick={() => step(1)} style={{ background: "none", border: "none", cursor: "pointer", color: "var(--text-secondary)", fontSize: 16, padding: "2px 8px" }}>›</button>
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "repeat(7, 1fr)", gap: 2, textAlign: "center" }}>
            {["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"].map((d) => (
              <div key={d} className="item-meta" style={{ padding: "4px 0" }}>{d}</div>
            ))}
            {cells.map((day, i) => {
              if (day == null) return <div key={`x-${i}`} />;
              const iso = toIsoDate(vy, vm, day);
              const sel = iso === value;
              const isToday = iso === todayIso;
              return (
                <button
                  key={iso}
                  type="button"
                  onClick={() => {
                    onChange(iso);
                    setOpen(false);
                  }}
                  className={sel ? "chip chip-active" : "chip"}
                  style={{
                    padding: "5px 0",
                    justifyContent: "center",
                    ...(isToday && !sel ? { borderColor: "var(--accent)" } : null),
                  }}
                >
                  {day}
                </button>
              );
            })}
          </div>
          <div style={{ display: "flex", justifyContent: "space-between", borderTop: "1px solid var(--border-subtle)", marginTop: 10, paddingTop: 8 }}>
            <button
              type="button"
              onClick={() => {
                onChange(toIsoDate(new Date().getFullYear(), new Date().getMonth(), new Date().getDate()));
                setOpen(false);
              }}
              style={{ background: "none", border: "none", cursor: "pointer", color: "var(--accent-hover)", fontSize: 13, fontWeight: 600, padding: "4px 8px" }}
            >
              Today
            </button>
            <button
              type="button"
              onClick={() => setOpen(false)}
              style={{ background: "none", border: "none", cursor: "pointer", color: "var(--accent-hover)", fontSize: 13, fontWeight: 600, padding: "4px 8px" }}
            >
              Close
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

function EventEditor({ ui, initial, onSave, onDelete, onClose }: {
  ui: PluginContext["ui"];
  initial: CalendarEvent;
  onSave: (e: CalendarEvent) => Promise<string | null>;
  onDelete?: () => void;
  onClose: () => void;
}) {
  const { FormField, Button, TextInput, Select } = ui;
  const [draft, setDraft] = useState<CalendarEvent>({ ...initial });
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const rule = repeatRule(draft);
  const canSave = !!draft.title.trim() && /^\d{4}-\d{2}-\d{2}$/.test(draft.date);

  const commit = async () => {
    if (!draft.title.trim()) {
      setError("Give the event a title.");
      return;
    }
    if (!canSave) {
      setError("Pick a valid date.");
      return;
    }
    setSaving(true);
    const err = await onSave({ ...draft, repeat: rule, repeat_yearly: rule === "yearly" });
    setSaving(false);
    if (err) setError(err);
    else onClose();
  };

  return (
    <ServerFormModal
      ui={ui}
      title={initial.id ? "Edit event" : "New event"}
      busy={saving}
      canSave={canSave}
      saveLabel="Save event"
      onSave={commit}
      onClose={onClose}
      danger={
        onDelete ? (
          <Button variant="danger" onClick={onDelete} disabled={saving}>
            Delete
          </Button>
        ) : undefined
      }
    >
      <FormField label="Title">
        <TextInput
          value={draft.title}
          maxLength={80}
          placeholder="e.g. Dentist appointment"
          onChange={(e: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, title: e.target.value })}
        />
      </FormField>
      <FormField label="Date" hint='Type it ("Dec 25", "tomorrow", "2026-12-25") or use the calendar.'>
        <DatePicker
          ui={ui}
          value={draft.date}
          onChange={(iso) => setDraft({ ...draft, date: iso })}
        />
      </FormField>
      <div className="st-field">
        <span className="st-field-label">All-day</span>
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={draft.all_day}
            onChange={(e) => setDraft({ ...draft, all_day: e.target.checked })}
          />
          <span className="item-meta">Whole day (fires 9am)</span>
        </label>
      </div>
      {!draft.all_day && (
        <div className="modal-grid-2">
          <FormField label="Time" hint='Type it ("3:00 pm", "15:00") or use the clock.'>
            <TimePicker
              ui={ui}
              hour={draft.hour}
              minute={draft.minute}
              onChange={(hour, minute) => setDraft({ ...draft, hour, minute })}
            />
          </FormField>
          <FormField label="Ends" hint="Optional, display only. Clear to remove.">
            <TimePicker
              ui={ui}
              hour={draft.end_hour}
              minute={draft.end_minute}
              placeholder="No end time"
              onChange={(h, m) => setDraft({ ...draft, end_hour: h, end_minute: m })}
              onClear={() => setDraft({ ...draft, end_hour: null, end_minute: null })}
            />
          </FormField>
        </div>
      )}
      <div className="st-field">
        <span className="st-field-label">Repeat</span>
        <Select
          aria-label="Repeat"
          value={rule}
          style={{ width: "100%" }}
          onChange={(e: React.ChangeEvent<HTMLSelectElement>) => setDraft({ ...draft, repeat: e.target.value as Repeat })}
        >
          {REPEAT_RULES.map((r) => (
            <option key={r} value={r}>
              {repeatLabel(r, draft.date)}
            </option>
          ))}
        </Select>
        <span className="st-field-hint">
          {rule === "none"
            ? "Fires once, on the date above."
            : "Repeats from the start date forward, at the event time."}
        </span>
      </div>
      <FormField label="Details">
        <TextInput
          value={draft.description}
          maxLength={280}
          placeholder="e.g. Bring insurance card"
          onChange={(e: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, description: e.target.value })}
        />
      </FormField>
      <FormField
        label="Spoken message"
        hint="Said out loud when the event fires. Empty = title only."
      >
        <TextInput
          value={draft.message}
          maxLength={280}
          placeholder="e.g. Dentist in thirty minutes"
          onChange={(e: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, message: e.target.value })}
        />
      </FormField>
      {error && <div className="error-banner">{error}</div>}
    </ServerFormModal>
  );
}

function EventRow({ ui, api, e, onEdit, onDelete, onToggle }: {
  ui: PluginContext["ui"];
  api: CalendarApi;
  e: CalendarEvent;
  onEdit: () => void;
  onDelete: () => void;
  onToggle: (id: string, enabled: boolean) => void;
}) {
  const { Button } = ui;
  const test = async () => {
    try {
      await api.test(e);
    } catch (err) {
      console.error("calendar test failed", err);
    }
  };

  return (
    <div className={`item-row${e.enabled ? "" : " item-row-dim"}`} style={{ display: "block" }}>
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <label title="Enable event" style={{ display: "flex", alignItems: "center", cursor: "pointer", flexShrink: 0 }}>
          <input
            type="checkbox"
            checked={e.enabled}
            onChange={(ev) => onToggle(e.id, ev.target.checked)}
          />
        </label>
        <div style={{ minWidth: 0, flex: 1 }}>
          <div className="item-title">
            📅 {eventLabel(e)}
            {e.title ? ` • ${e.title}` : ""}
          </div>
          <div className="item-meta">
            {eventStatus(e)}
            {e.end_hour != null && e.end_minute != null && !e.all_day
              ? ` • ends ${String(e.end_hour).padStart(2, "0")}:${String(e.end_minute).padStart(2, "0")}`
              : ""}
          </div>
        </div>
        <div style={{ display: "flex", gap: 6, flexShrink: 0 }}>
          <Button variant="primary" onClick={test} style={{ padding: "5px 10px", fontSize: 11 }}>
            Test
          </Button>
          <Button variant="secondary" onClick={onEdit} style={{ padding: "5px 10px", fontSize: 11 }}>
            Edit
          </Button>
          <Button variant="danger" onClick={onDelete} style={{ padding: "5px 10px", fontSize: 11 }}>
            Delete
          </Button>
        </div>
      </div>
      {!e.enabled && <div className="item-meta">paused</div>}
      {e.description && <div className="item-meta">{e.description}</div>}
      {e.message && <div className="item-meta">says: {e.message}</div>}
    </div>
  );
}

export function CalendarSection({ ctx }: { ctx: PluginContext }) {
  const { ui } = ctx;
  const { events, error, save, remove, toggle, api } = useCalendar(ctx);
  const { SectionCard, Button } = ui;
  // `id === ""` marks a new-event draft (backend assigns the id on save).
  const [editing, setEditing] = useState<CalendarEvent | null>(null);
  const [pendingDelete, setPendingDelete] = useState<CalendarEvent | null>(null);
  const [deleting, setDeleting] = useState(false);

  const enabled = events.filter((e) => e.enabled).length;
  const sorted = sortEvents(events);

  const confirmDelete = async () => {
    if (!pendingDelete) return;
    setDeleting(true);
    await remove(pendingDelete.id);
    setPendingDelete(null);
    setDeleting(false);
  };

  return (
    <SectionCard
      title="Calendar"
      badge={events.length ? `${enabled}/${events.length} on` : undefined}
    >
      <CalendarMonthView
        events={events}
        onEdit={(e) => setEditing(e)}
        onCreateDate={(iso) => setEditing({ ...emptyEvent(), date: iso })}
      />
      <div style={{ marginBottom: 10 }}>
        {sorted.map((e) => (
          <EventRow
            key={e.id}
            ui={ui}
            api={api}
            e={e}
            onEdit={() => setEditing(e)}
            onDelete={() => setPendingDelete(e)}
            onToggle={toggle}
          />
        ))}
        {!sorted.length && (
          <div className="item-meta" style={{ marginBottom: 8 }}>
            No events yet. Click a date on the calendar to add one, or use “+ Add event”.
          </div>
        )}
      </div>

      {editing ? (
        <EventEditor
          key={editing.id || "new"}
          ui={ui}
          initial={editing}
          onSave={save}
          onClose={() => setEditing(null)}
          onDelete={
            editing.id
              ? () => {
                  const doomed = editing;
                  setEditing(null);
                  setPendingDelete(doomed);
                }
              : undefined
          }
        />
      ) : (
        <Button
          variant="primary"
          onClick={() => setEditing(emptyEvent())}
          style={{ width: "100%" }}
        >
          + Add event
        </Button>
      )}

      {error && <div className="error-banner">{error}</div>}

      {pendingDelete && (
        <ConfirmModal
          ui={ui}
          title="Delete event?"
          message={`Delete "${pendingDelete.title || eventLabel(pendingDelete)}"? This cannot be undone.`}
          busy={deleting}
          onClose={() => !deleting && setPendingDelete(null)}
          onConfirm={confirmDelete}
        />
      )}
    </SectionCard>
  );
}
