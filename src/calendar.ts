/// Google-style repeat rule. `weekly`/`monthly`/`yearly` derive their
/// pattern from the event's start date ("Weekly on Tuesday").
export type Repeat = "none" | "daily" | "weekdays" | "weekly" | "monthly" | "yearly";

export type CalendarEvent = {
  id: string;
  title: string;
  description: string;
  date: string;
  hour: number;
  minute: number;
  all_day: boolean;
  end_hour?: number | null;
  end_minute?: number | null;
  /** Repeat rule. Absent on old saved events: falls back to repeat_yearly. */
  repeat?: Repeat | null;
  /// Legacy flag kept in sync with `repeat === "yearly"`.
  repeat_yearly: boolean;
  message: string;
  enabled: boolean;
  last_fired?: number | null;
};

export type Repeatable = Pick<
  CalendarEvent,
  "date" | "hour" | "minute" | "all_day" | "end_hour" | "end_minute" | "repeat" | "repeat_yearly"
>;

/// The effective repeat rule, mapping old events that only carry the
/// legacy `repeat_yearly` bool.
export function repeatRule(e: Pick<CalendarEvent, "repeat" | "repeat_yearly">): Repeat {
  return e.repeat ?? (e.repeat_yearly ? "yearly" : "none");
}

const WEEKDAY_LONG = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

function isoParts(iso: string): { y: number; mo: number; d: number } | null {
  const m = iso.match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!m) return null;
  return { y: Number(m[1]), mo: Number(m[2]) - 1, d: Number(m[3]) };
}

/// Google-style dynamic repeat label for the dropdown, derived from the
/// event's start date ("Weekly on Tuesday", "Annually on Dec 25").
export function repeatLabel(rule: Repeat, iso: string): string {
  const p = isoParts(iso);
  switch (rule) {
    case "daily":
      return "Daily";
    case "weekdays":
      return "Every weekday (Mon–Fri)";
    case "weekly":
      return p ? `Weekly on ${WEEKDAY_LONG[new Date(p.y, p.mo, p.d).getDay()]}` : "Weekly";
    case "monthly":
      return p ? `Monthly on day ${p.d}` : "Monthly";
    case "yearly":
      return p
        ? `Annually on ${new Date(p.y, p.mo, p.d).toLocaleDateString([], { month: "short", day: "numeric" })}`
        : "Annually";
    case "none":
      return "Does not repeat";
  }
}

/// Compact lowercase label for list rows; empty for non-repeating events.
export function repeatShort(rule: Repeat, iso: string): string {
  const p = isoParts(iso);
  const shortWeekday = p ? ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][new Date(p.y, p.mo, p.d).getDay()] : "";
  const shortDate = p
    ? new Date(p.y, p.mo, p.d).toLocaleDateString([], { month: "short", day: "numeric" })
    : "";
  switch (rule) {
    case "daily":
      return "daily";
    case "weekdays":
      return "weekdays";
    case "weekly":
      return `weekly on ${shortWeekday}`;
    case "monthly":
      return p ? `monthly on day ${p.d}` : "monthly";
    case "yearly":
      return `yearly on ${shortDate}`;
    case "none":
      return "";
  }
}

export function emptyEvent(date = new Date()): CalendarEvent {
  const iso = `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
  return {
    id: "",
    title: "",
    description: "",
    date: iso,
    hour: 9,
    minute: 0,
    all_day: false,
    end_hour: null,
    end_minute: null,
    repeat: "none",
    repeat_yearly: false,
    message: "",
    enabled: true,
  };
}

export function timeLabel(hour: number, minute: number): string {
  return `${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}`;
}

export function dateLabel(iso: string): string {
  const m = iso.match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!m) return iso;
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
  return d.toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" });
}

export function eventLabel(e: Pick<CalendarEvent, "date" | "hour" | "minute" | "all_day">): string {
  const date = dateLabel(e.date);
  return e.all_day ? `${date} • all day` : `${date} • ${timeLabel(e.hour, e.minute)}`;
}

export function eventStatus(
  e: Pick<CalendarEvent, "date" | "repeat" | "repeat_yearly">,
  now = new Date(),
): string {
  const p = isoParts(e.date);
  if (!p) return e.date;
  const d = new Date(p.y, p.mo, p.d);
  const startOf = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diff = Math.round((startOf(d) - startOf(now)) / 86400_000);
  const rel =
    diff < 0 ? "Past" : diff === 0 ? "Today" : diff === 1 ? "Tomorrow" : d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
  const short = repeatShort(repeatRule(e), e.date);
  return short ? `${rel} • ${short}` : rel;
}

export function sortEvents<T extends Pick<CalendarEvent, "date" | "hour" | "minute" | "title">>(list: T[]): T[] {
  return [...list].sort(
    (x, y) => x.date.localeCompare(y.date) || x.hour - y.hour || x.minute - y.minute || x.title.localeCompare(y.title),
  );
}

export function toIsoDate(year: number, month: number, day: number): string {
  return `${year}-${String(month + 1).padStart(2, "0")}-${String(day).padStart(2, "0")}`;
}

/// Lenient date entry for the custom picker text field: ISO, slashes,
/// "today"/"tomorrow", "Dec 25", weekday names. Returns ISO or null.
export function parseDateInput(raw: string, today = new Date()): string | null {
  const s = raw.trim();
  if (!s) return null;
  const lower = s.toLowerCase();
  const base = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  if (lower === "today") return toIsoDate(base.getFullYear(), base.getMonth(), base.getDate());
  if (lower === "tomorrow") {
    const t = new Date(base);
    t.setDate(t.getDate() + 1);
    return toIsoDate(t.getFullYear(), t.getMonth(), t.getDate());
  }
  let m = s.match(/^(\d{4})[-\/](\d{1,2})[-\/](\d{1,2})$/);
  if (m) {
    const y = Number(m[1]);
    const mo = Number(m[2]);
    const d = Number(m[3]);
    if (mo < 1 || mo > 12 || d < 1 || d > 31) return null;
    const dt = new Date(y, mo - 1, d);
    if (dt.getMonth() !== mo - 1) return null;
    return toIsoDate(y, mo - 1, d);
  }
  m = s.match(/^(\d{1,2})[\/\-](\d{1,2})$/);
  if (m) {
    const mo = Number(m[1]);
    const d = Number(m[2]);
    if (mo < 1 || mo > 12 || d < 1) return null;
    const dt = new Date(base.getFullYear(), mo - 1, d);
    if (dt.getMonth() !== mo - 1) return null;
    return toIsoDate(dt.getFullYear(), mo - 1, d);
  }
  const months: Record<string, number> = {
    jan: 0, feb: 1, mar: 2, apr: 3, may: 4, jun: 5,
    jul: 6, aug: 7, sep: 8, oct: 9, nov: 10, dec: 11,
  };
  let mo: number | null = null;
  let d: number | null = null;
  let words = 0;
  for (const token of lower.split(/[^a-z0-9]+/)) {
    if (!token) continue;
    words++;
    const key = token.slice(0, 3);
    if (key in months && mo == null) mo = months[key];
    else if (/^\d{1,2}$/.test(token)) {
      const n = Number(token);
      if (n >= 1 && n <= 31 && d == null) d = n;
    }
  }
  if (mo != null && d != null && words <= 3) {
    let dt = new Date(base.getFullYear(), mo, d);
    if (dt.getMonth() !== mo) return null;
    if (dt < base) dt = new Date(base.getFullYear() + 1, mo, d);
    return toIsoDate(dt.getFullYear(), dt.getMonth(), dt.getDate());
  }
  const days = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
  if (/^[a-z]+$/.test(lower)) {
    const target = days.indexOf(lower.slice(0, 3));
    if (target >= 0) {
      const todayIdx = (base.getDay() + 6) % 7;
      let offset = (target + 7 - todayIdx) % 7;
      if (offset === 0) offset = 7;
      const dt = new Date(base);
      dt.setDate(dt.getDate() + offset);
      return toIsoDate(dt.getFullYear(), dt.getMonth(), dt.getDate());
    }
  }
  return null;
}

export type EventOccurrence<T> = { event: T; start: Date; end: Date };

// Hard cap on day iterations per event: the widest caller window is a
// padded month (~45 days); this just bounds pathological ranges.
const MAX_RANGE_DAYS = 800;

function toOccurrence<T extends Repeatable>(e: T, d: Date): EventOccurrence<T> {
  const s = e.all_day
    ? new Date(d.getFullYear(), d.getMonth(), d.getDate(), 9, 0, 0)
    : new Date(d.getFullYear(), d.getMonth(), d.getDate(), e.hour, e.minute, 0);
  const en =
    e.end_hour != null && e.end_minute != null && !e.all_day
      ? new Date(d.getFullYear(), d.getMonth(), d.getDate(), e.end_hour, e.end_minute, 0)
      : new Date(s.getTime() + (e.all_day ? 24 * 3600_000 : 3600_000));
  return { event: e, start: s, end: en };
}

/// True when `cursor` is an occurrence day of `first` under `rule`.
/// All non-yearly rules only run from the first occurrence forward.
function matchesRule(cursor: Date, first: Date, rule: Repeat): boolean {
  switch (rule) {
    case "daily":
      return cursor.getTime() >= first.getTime();
    case "weekdays":
      return cursor.getTime() >= first.getTime() && cursor.getDay() >= 1 && cursor.getDay() <= 5;
    case "weekly":
      return cursor.getTime() >= first.getTime() && cursor.getDay() === first.getDay();
    case "monthly":
      return cursor.getTime() >= first.getTime() && cursor.getDate() === first.getDate();
    case "yearly":
      return cursor.getMonth() === first.getMonth() && cursor.getDate() === first.getDate();
    case "none":
      return cursor.getTime() === first.getTime();
  }
}

/// Expands each event into concrete occurrences within [start, end],
/// materializing repeat rules (daily/weekdays/weekly/monthly/yearly).
export function eventOccurrencesInRange<T extends Repeatable>(
  list: T[],
  start: Date,
  end: Date,
): EventOccurrence<T>[] {
  const out: EventOccurrence<T>[] = [];
  const dayStart = new Date(start.getFullYear(), start.getMonth(), start.getDate()).getTime();
  const dayEnd = new Date(end.getFullYear(), end.getMonth(), end.getDate() + 1).getTime();
  for (const e of list) {
    const p = isoParts(e.date);
    if (!p) continue;
    const first = new Date(p.y, p.mo, p.d);
    if (first.getMonth() !== p.mo || first.getDate() !== p.d) continue;
    const rule = repeatRule(e);
    if (rule === "yearly") {
      // Birthday-style: month/day in every year the window touches.
      for (let y = start.getFullYear(); y <= end.getFullYear(); y++) {
        const d = new Date(y, p.mo, p.d);
        if (d.getMonth() !== p.mo || d.getDate() !== p.d) continue;
        if (d.getTime() < dayStart || d.getTime() >= dayEnd) continue;
        out.push(toOccurrence(e, d));
      }
      continue;
    }
    const cursor =
      first.getTime() > dayStart
        ? new Date(first.getFullYear(), first.getMonth(), first.getDate())
        : new Date(start.getFullYear(), start.getMonth(), start.getDate());
    for (let i = 0; i < MAX_RANGE_DAYS && cursor.getTime() < dayEnd; i++) {
      if (matchesRule(cursor, first, rule) && cursor.getTime() >= dayStart) {
        out.push(toOccurrence(e, cursor));
      }
      cursor.setDate(cursor.getDate() + 1);
    }
  }
  return out.sort((a, b) => a.start.getTime() - b.start.getTime());
}

export function eventOccurrencesInMonth<T extends Repeatable>(
  list: T[],
  year: number,
  month: number,
): EventOccurrence<T>[] {
  return eventOccurrencesInRange(list, new Date(year, month, 1), new Date(year, month + 1, 0));
}
