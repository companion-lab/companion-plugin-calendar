import { describe, expect, it } from "vitest";
import {
  emptyEvent,
  eventOccurrencesInMonth,
  eventOccurrencesInRange,
  eventStatus,
  parseDateInput,
  repeatLabel,
  repeatRule,
  repeatShort,
  sortEvents,
  timeLabel,
  toIsoDate,
  type CalendarEvent,
} from "./calendar";

// Fixed window: Mon Jan 12 – Sun Jan 18 2026 (occurrence days Jan 12..18).
// Mid-January is DST-free in every timezone, so local-midnight day math and
// the +24h all-day end stay exact.
const W0 = new Date(2026, 0, 12);
const W1 = new Date(2026, 0, 18);
// Jan 15 2026 is a Thursday; used as "today" for parseDateInput/eventStatus.
const today = new Date(2026, 0, 15);

const at = (y: number, mo: number, d: number, h = 0, mi = 0) => new Date(y, mo, d, h, mi).getTime();

function makeEvent(partial: Partial<CalendarEvent> & { date: string }): CalendarEvent {
  return {
    id: "e",
    title: "E",
    description: "",
    date: partial.date,
    hour: 9,
    minute: 0,
    all_day: false,
    end_hour: null,
    end_minute: null,
    repeat: "none",
    repeat_yearly: false,
    message: "",
    enabled: true,
    ...partial,
  };
}

describe("repeatRule", () => {
  it("uses the repeat field when present", () => {
    expect(repeatRule({ repeat: "daily", repeat_yearly: false })).toBe("daily");
    // "none" is a real value, not a nullish one — the legacy flag is ignored.
    expect(repeatRule({ repeat: "none", repeat_yearly: true })).toBe("none");
  });

  it("falls back to the legacy repeat_yearly flag on old events", () => {
    expect(repeatRule({ repeat: null, repeat_yearly: true })).toBe("yearly");
    expect(repeatRule({ repeat: null, repeat_yearly: false })).toBe("none");
    expect(repeatRule({ repeat: undefined, repeat_yearly: true })).toBe("yearly");
  });
});

describe("repeatLabel", () => {
  it("renders the fixed labels", () => {
    expect(repeatLabel("none", "2026-01-13")).toBe("Does not repeat");
    expect(repeatLabel("daily", "2026-01-13")).toBe("Daily");
    expect(repeatLabel("weekdays", "2026-01-13")).toBe("Every weekday (Mon–Fri)");
  });

  it("derives weekly/monthly patterns from the start date", () => {
    // Jan 13 2026 is a Tuesday.
    expect(repeatLabel("weekly", "2026-01-13")).toBe("Weekly on Tuesday");
    expect(repeatLabel("monthly", "2026-01-25")).toBe("Monthly on day 25");
  });

  it("formats yearly via the locale (mirrored) and falls back on bad dates", () => {
    // The month/day rendering comes from toLocaleDateString in the source;
    // mirror that exact call so the assertion holds in any locale.
    const short = new Date(2026, 0, 13).toLocaleDateString([], { month: "short", day: "numeric" });
    expect(repeatLabel("yearly", "2026-01-13")).toBe(`Annually on ${short}`);
    // Unparseable ISO (strict 2-digit regex) drops the pattern suffix.
    expect(repeatLabel("weekly", "nope")).toBe("Weekly");
    expect(repeatLabel("monthly", "2026-1-5")).toBe("Monthly");
    expect(repeatLabel("yearly", "")).toBe("Annually");
  });
});

describe("repeatShort", () => {
  it("is empty for non-repeating and plain for daily/weekdays", () => {
    expect(repeatShort("none", "2026-01-13")).toBe("");
    expect(repeatShort("daily", "2026-01-13")).toBe("daily");
    expect(repeatShort("weekdays", "2026-01-13")).toBe("weekdays");
  });

  it("compacts the pattern using the start date", () => {
    expect(repeatShort("weekly", "2026-01-13")).toBe("weekly on Tue"); // Jan 13 = Tuesday
    expect(repeatShort("monthly", "2026-01-25")).toBe("monthly on day 25");
    const short = new Date(2026, 0, 13).toLocaleDateString([], { month: "short", day: "numeric" });
    expect(repeatShort("yearly", "2026-01-13")).toBe(`yearly on ${short}`);
  });
});

describe("small model helpers", () => {
  it("emptyEvent stamps the given date at 09:00, non-repeating", () => {
    expect(emptyEvent(new Date(2026, 0, 15))).toEqual({
      id: "",
      title: "",
      description: "",
      date: "2026-01-15",
      hour: 9,
      minute: 0,
      all_day: false,
      end_hour: null,
      end_minute: null,
      repeat: "none",
      repeat_yearly: false,
      message: "",
      enabled: true,
    });
  });

  it("timeLabel zero-pads", () => {
    expect(timeLabel(14, 5)).toBe("14:05");
    expect(timeLabel(9, 0)).toBe("09:00");
  });

  it("toIsoDate takes a 0-based month and pads both fields", () => {
    expect(toIsoDate(2026, 0, 5)).toBe("2026-01-05");
    expect(toIsoDate(2026, 11, 31)).toBe("2026-12-31");
  });
});

describe("sortEvents", () => {
  it("orders by date, then hour, then minute, then title — without mutating", () => {
    const c = makeEvent({ id: "c", title: "b", date: "2026-01-15", hour: 10, minute: 0 });
    const p = makeEvent({ id: "p", title: "p", date: "2026-01-15", hour: 9, minute: 30 });
    const z = makeEvent({ id: "z", title: "z", date: "2026-01-14", hour: 23, minute: 59 });
    const a = makeEvent({ id: "a", title: "a", date: "2026-01-15", hour: 10, minute: 0 });
    const list = [c, p, z, a];
    const sorted = sortEvents(list);
    expect(sorted.map((e) => e.id)).toEqual(["z", "p", "a", "c"]);
    // sortEvents copies; the caller's array keeps its original order.
    expect(list.map((e) => e.id)).toEqual(["c", "p", "z", "a"]);
  });
});

describe("parseDateInput", () => {
  const todayIso = "2026-01-15";

  it("parses relative words against the fixed today", () => {
    expect(parseDateInput("today", today)).toBe(todayIso);
    expect(parseDateInput("  TODAY  ", today)).toBe(todayIso); // trimmed + lowercased
    expect(parseDateInput("tomorrow", today)).toBe("2026-01-16");
    // setDate overflow: "tomorrow" from Jan 31 lands on Feb 1.
    expect(parseDateInput("tomorrow", new Date(2026, 0, 31))).toBe("2026-02-01");
  });

  it("parses ISO and slashed dates, rejecting impossible ones", () => {
    expect(parseDateInput("2026-01-25", today)).toBe("2026-01-25");
    expect(parseDateInput("2026/1/5", today)).toBe("2026-01-05"); // 1-digit allowed
    expect(parseDateInput("2026-13-01", today)).toBeNull(); // month > 12
    expect(parseDateInput("2026-02-30", today)).toBeNull(); // normalizes to Mar 2 → rejected
  });

  it("parses bare month/day into the current year without rolling forward", () => {
    expect(parseDateInput("1-25", today)).toBe("2026-01-25");
    // Unlike month names, numeric input keeps past dates as-is.
    expect(parseDateInput("1-10", today)).toBe("2026-01-10");
    expect(parseDateInput("13-05", today)).toBeNull();
    expect(parseDateInput("0-10", today)).toBeNull();
    expect(parseDateInput("2-30", today)).toBeNull(); // Feb 30 → Mar 2 → rejected
  });

  it("parses month names, rolling to next year when already past", () => {
    expect(parseDateInput("Dec 25", today)).toBe("2026-12-25");
    // Jan 10 is before the Jan 15 "today" → next occurrence is 2027.
    expect(parseDateInput("Jan 10", today)).toBe("2027-01-10");
    expect(parseDateInput("february 15", today)).toBe("2026-02-15");
    expect(parseDateInput("Feb 30", today)).toBeNull();
  });

  it("parses weekday names as the next occurrence, always ahead", () => {
    // today is Thursday: Friday is +1, Monday is +4.
    expect(parseDateInput("friday", today)).toBe("2026-01-16");
    expect(parseDateInput("monday", today)).toBe("2026-01-19");
    // Same weekday as today is forced a full week ahead (offset 0 → 7).
    expect(parseDateInput("thursday", today)).toBe("2026-01-22");
    expect(parseDateInput("xyz", today)).toBeNull();
    expect(parseDateInput("25", today)).toBeNull(); // day without month
    expect(parseDateInput("", today)).toBeNull();
    expect(parseDateInput("   ", today)).toBeNull();
  });
});

describe("eventStatus", () => {
  it("labels today/tomorrow/past plus the short repeat tag", () => {
    expect(eventStatus({ date: "2026-01-15", repeat: "daily", repeat_yearly: false }, today)).toBe("Today • daily");
    expect(eventStatus({ date: "2026-01-16", repeat: "daily", repeat_yearly: false }, today)).toBe("Tomorrow • daily");
    expect(eventStatus({ date: "2026-01-14", repeat: "daily", repeat_yearly: false }, today)).toBe("Past • daily");
  });

  it("drops the separator for non-repeating events and passes junk through", () => {
    expect(eventStatus({ date: "2026-01-15", repeat: "none", repeat_yearly: false }, today)).toBe("Today");
    expect(eventStatus({ date: "not-a-date", repeat: "daily", repeat_yearly: false }, today)).toBe("not-a-date");
  });
});

describe("eventOccurrencesInRange — none", () => {
  it("yields exactly one occurrence inside the window and none outside", () => {
    const list = [
      makeEvent({ id: "in", date: "2026-01-14", hour: 10, minute: 0, repeat: "none" }),
      // A past "none" event never matches again (rule is exact-equality)…
      makeEvent({ id: "past", date: "2025-12-25", hour: 9, minute: 0, repeat: "none" }),
      // …and one beyond the window is out of range.
      makeEvent({ id: "future", date: "2026-01-20", hour: 9, minute: 0, repeat: "none" }),
    ];
    const out = eventOccurrencesInRange(list, W0, W1);
    expect(out.map((o) => o.event.id)).toEqual(["in"]);
    expect(out[0].start.getTime()).toBe(at(2026, 0, 14, 10, 0));
    expect(out[0].end.getTime()).toBe(at(2026, 0, 14, 11, 0)); // default +1h end
  });
});

describe("eventOccurrencesInRange — daily", () => {
  it("fills every day from the event's first date to the window end", () => {
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "d", date: "2026-01-14", hour: 9, minute: 0, repeat: "daily" })],
      W0,
      W1,
    );
    // Jan 14 is the first occurrence, so Jan 12–13 are not filled.
    expect(out.map((o) => o.start.getTime())).toEqual(
      [14, 15, 16, 17, 18].map((d) => at(2026, 0, d, 9, 0)),
    );
  });

  it("fills the whole window when the event started before it", () => {
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "d2", date: "2026-01-10", hour: 8, minute: 30, repeat: "daily" })],
      W0,
      W1,
    );
    expect(out.map((o) => o.start.getTime())).toEqual(
      [12, 13, 14, 15, 16, 17, 18].map((d) => at(2026, 0, d, 8, 30)),
    );
  });
});

describe("eventOccurrencesInRange — weekdays", () => {
  it("keeps Mon–Fri and skips Sat/Sun", () => {
    // Jan 14 2026 is a Wednesday: Wed/Thu/Fri pass, Sat 17 + Sun 18 skip.
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "w", date: "2026-01-14", hour: 7, minute: 15, repeat: "weekdays" })],
      W0,
      W1,
    );
    expect(out.map((o) => o.start.getTime())).toEqual(
      [14, 15, 16].map((d) => at(2026, 0, d, 7, 15)),
    );
  });

  it("repeats into the window when started on a weekend before it", () => {
    // Jan 10 is a Saturday: cursor starts at the window's Monday Jan 12.
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "w2", date: "2026-01-10", hour: 9, minute: 0, repeat: "weekdays" })],
      W0,
      W1,
    );
    expect(out.map((o) => o.start.getTime())).toEqual(
      [12, 13, 14, 15, 16].map((d) => at(2026, 0, d, 9, 0)),
    );
  });
});

describe("eventOccurrencesInRange — weekly", () => {
  it("matches only the start date's weekday inside the window", () => {
    // Jan 13 is a Tuesday; within Mon Jan 12 – Sun Jan 18 only Jan 13 hits.
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "wk", date: "2026-01-13", hour: 9, minute: 0, repeat: "weekly" })],
      W0,
      W1,
    );
    expect(out.map((o) => o.start.getTime())).toEqual([at(2026, 0, 13, 9, 0)]);
  });

  it("keeps repeating into later windows from an earlier start", () => {
    // Dec 30 2025 is also a Tuesday; Tue Jan 13 2026 still matches.
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "wk2", date: "2025-12-30", hour: 9, minute: 0, repeat: "weekly" })],
      W0,
      W1,
    );
    expect(out.map((o) => o.start.getTime())).toEqual([at(2026, 0, 13, 9, 0)]);
  });
});

describe("eventOccurrencesInRange — monthly", () => {
  it("skips short months instead of clamping to month end", () => {
    // Day-31 monthly over Jan–Apr 2026: Feb (28d) and Apr (30d) have no
    // 31st, so only Jan 31 and Mar 31 occur — no Feb 29/30/31 fabrication.
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "m31", date: "2026-01-31", hour: 20, minute: 0, repeat: "monthly" })],
      new Date(2026, 0, 1),
      new Date(2026, 3, 30),
    );
    expect(out.map((o) => o.start.getTime())).toEqual([
      at(2026, 0, 31, 20, 0),
      at(2026, 2, 31, 20, 0),
    ]);
  });
});

describe("eventOccurrencesInRange — yearly", () => {
  it("matches month/day in every year the window touches", () => {
    // First occurrence dated 2025; the Dec 2026 window still gets Dec 25.
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "x", date: "2025-12-25", hour: 9, minute: 0, repeat: "yearly" })],
      new Date(2026, 11, 20),
      new Date(2026, 11, 31),
    );
    expect(out.map((o) => o.start.getTime())).toEqual([at(2026, 11, 25, 9, 0)]);
  });

  it("drives off the legacy repeat_yearly flag when repeat is absent", () => {
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "legacy", date: "2025-12-25", hour: 9, minute: 0, repeat: undefined, repeat_yearly: true })],
      new Date(2026, 11, 20),
      new Date(2026, 11, 31),
    );
    expect(out.map((o) => o.event.id)).toEqual(["legacy"]);
  });

  it("keeps Feb 29 in leap years and drops the roll-over day in others", () => {
    const leap = [makeEvent({ id: "leap", date: "2024-02-29", all_day: true, repeat: "yearly" })];
    // 2028 is a leap year → real Feb 29 occurrence (all-day 09:00 slot).
    // The end date is inclusive, so it must be Feb 29 itself to cover it.
    const in2028 = eventOccurrencesInRange(leap, new Date(2028, 1, 1), new Date(2028, 1, 29));
    expect(in2028.map((o) => o.start.getTime())).toEqual([at(2028, 1, 29, 9, 0)]);
    // 2027 is not: Feb 29 normalizes to Mar 1 and is rejected by the
    // month guard, so nothing occurs even though Mar 1 is in range.
    const in2027 = eventOccurrencesInRange(leap, new Date(2027, 1, 1), new Date(2027, 2, 15));
    expect(in2027).toEqual([]);
  });
});

describe("occurrence start/end times", () => {
  it("all-day events use the fixed 09:00 slot and ignore end fields", () => {
    const out = eventOccurrencesInRange(
      [makeEvent({ id: "ad", date: "2026-01-14", all_day: true, end_hour: 16, end_minute: 30, repeat: "none" })],
      W0,
      W1,
    );
    expect(out[0].start.getTime()).toBe(at(2026, 0, 14, 9, 0));
    // End is start + 24h — end_hour/end_minute only apply to timed events.
    expect(out[0].end.getTime()).toBe(at(2026, 0, 15, 9, 0));
  });

  it("timed events honor a complete end pair and default to +1h otherwise", () => {
    const full = eventOccurrencesInRange(
      [makeEvent({ id: "f", date: "2026-01-14", hour: 10, minute: 0, end_hour: 12, end_minute: 45, repeat: "none" })],
      W0,
      W1,
    );
    expect(full[0].end.getTime()).toBe(at(2026, 0, 14, 12, 45));
    // end_minute null → the pair is incomplete → default one-hour duration.
    const partial = eventOccurrencesInRange(
      [makeEvent({ id: "p", date: "2026-01-14", hour: 10, minute: 0, end_hour: 12, end_minute: null, repeat: "none" })],
      W0,
      W1,
    );
    expect(partial[0].end.getTime()).toBe(at(2026, 0, 14, 11, 0));
  });
});

describe("eventOccurrencesInRange — ordering and validation", () => {
  it("sorts occurrences by start time regardless of input order", () => {
    const out = eventOccurrencesInRange(
      [
        makeEvent({ id: "late", date: "2026-01-16", hour: 9, minute: 0, repeat: "daily" }),
        makeEvent({ id: "night", date: "2026-01-14", hour: 23, minute: 0, repeat: "none" }),
      ],
      W0,
      W1,
    );
    // night (Jan 14 23:00) before the three "late" days (Jan 16–18).
    expect(out.map((o) => o.event.id)).toEqual(["night", "late", "late", "late"]);
    expect(out.map((o) => o.start.getTime())).toEqual([
      at(2026, 0, 14, 23, 0),
      at(2026, 0, 16, 9, 0),
      at(2026, 0, 17, 9, 0),
      at(2026, 0, 18, 9, 0),
    ]);
  });

  it("silently drops events with invalid or non-normalizing dates", () => {
    const list = [
      makeEvent({ id: "unpadded", date: "2026-1-14", repeat: "daily" }), // regex wants 2-digit fields
      makeEvent({ id: "overflow", date: "2026-02-30", repeat: "daily" }), // normalizes to Mar 2
      makeEvent({ id: "junk", date: "not-a-date", repeat: "daily" }),
    ];
    // A daily event would otherwise fill the whole quarter — empty means
    // all three were rejected before expansion.
    expect(eventOccurrencesInRange(list, new Date(2026, 0, 1), new Date(2026, 2, 31))).toEqual([]);
  });
});

describe("eventOccurrencesInMonth", () => {
  it("expands across the whole calendar month", () => {
    // Tuesday weekly over January 2026: Jan 13, 20, 27.
    const out = eventOccurrencesInMonth(
      [makeEvent({ id: "wk", date: "2026-01-13", hour: 9, minute: 0, repeat: "weekly" })],
      2026,
      0,
    );
    expect(out.map((o) => o.start.getTime())).toEqual(
      [13, 20, 27].map((d) => at(2026, 0, d, 9, 0)),
    );
  });
});
