// Calendar feature plugin: settings tab, reminder banner widget, and calendar
// tools (registered on the Rust backend side via `tools()`).

import { definePlugin, type PluginContext } from "@companion-lab/plugin";
import { CalendarSection } from "./CalendarSection";
import { ReminderBanner } from "./ReminderBanner";

export default definePlugin((ctx: PluginContext) => {
  ctx.log.info("activating calendar plugin");

  // Settings: same tab and section ids the core used, so overrides and user
  // habits carry over unchanged.
  ctx.settings.registerTab({
    id: "calendar",
    label: "Calendar",
    group: "Companion",
    order: 30,
    desc: "Dated events, birthdays, and appointments.",
    icon: ({ className }: { className?: string }) => (
      <svg className={className} width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><rect x="3" y="4" width="18" height="18" rx="2" ry="2" /><line x1="16" y1="2" x2="16" y2="6" /><line x1="8" y1="2" x2="8" y2="6" /><line x1="3" y1="10" x2="21" y2="10" /></svg>
    ),
  });
  ctx.settings.registerSection({
    id: "calendar.calendar",
    tabId: "calendar",
    slot: "top",
    order: 100,
    component: function CalendarSectionViaCtx() {
      return <CalendarSection ctx={ctx} />;
    },
  });

  // Main window: the reminder banner (announces via ctx.services.tts(),
  // drives the avatar via ctx.avatar).
  ctx.main.registerWidget({
    slot: "main.bottom",
    component: function CalendarReminderBanner() {
      return <ReminderBanner ctx={ctx} />;
    },
  });
});
