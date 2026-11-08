// Backend access layer: typed wrappers over ctx.backend.call plus a hook
// replacement for the former host useCalendar().

import { useCallback, useEffect, useState } from "react";
import type { PluginContext } from "@companion-lab/plugin";
import type { CalendarEvent } from "./calendar";

type ListReply = { events: CalendarEvent[] };

export function makeCalendarApi(ctx: PluginContext) {
  return {
    list: () => ctx.backend.call<ListReply>("calendar.list").then((r) => r.events),
    save: (event: CalendarEvent) =>
      ctx.backend
        .call<ListReply>("calendar.save", { event })
        .then((r) => r.events),
    remove: (id: string) =>
      ctx.backend
        .call<ListReply>("calendar.delete", { id })
        .then((r) => r.events),
    toggle: (id: string, enabled: boolean) =>
      ctx.backend
        .call<ListReply>("calendar.toggle", { id, enabled })
        .then((r) => r.events),
    // Former host `alarm_test` command: it only emitted the same
    // `alarm-fired` payload a scheduler tick would, so the plugin equivalent
    // is emitting that event (namespaced to this plugin, where the
    // ReminderBanner in the main window is listening).
    test: (e: Pick<CalendarEvent, "title" | "message">) =>
      ctx.events.emit("alarm-fired", {
        id: "test",
        label: e.title,
        message: e.message,
        loop_sound: false,
      }),
  };
}

export type CalendarApi = ReturnType<typeof makeCalendarApi>;

/** Former host useCalendar(), now backed by the plugin backend. */
export function useCalendar(ctx: PluginContext) {
  const [events, setEvents] = useState<CalendarEvent[]>([]);
  const [error, setError] = useState<string | null>(null);
  // The api object is cheap; keep a stable ref so hooks below stay put.
  const api = makeCalendarApi(ctx);

  const refresh = useCallback(async () => {
    try {
      setEvents(await api.list());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
    // Backend pushes this whenever the stored list changes (any window).
    return ctx.events.on("calendar-changed", () => void refresh());
  }, [refresh, ctx]);

  const save = useCallback(
    async (e: CalendarEvent) => {
      try {
        setEvents(await api.save(e));
        return null;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        setError(msg);
        return msg;
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  const remove = useCallback(async (id: string) => {
    try {
      setEvents(await api.remove(id));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const toggle = useCallback(async (id: string, enabled: boolean) => {
    try {
      setEvents(await api.toggle(id, enabled));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return { events, error, save, remove, toggle, refresh, api };
}
