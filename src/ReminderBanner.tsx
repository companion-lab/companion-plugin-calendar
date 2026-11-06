// Reminder banner for the main (avatar) window, shown when a calendar event
// fires. Port of the alarms RingBanner: listen for `alarm-fired` from this
// plugin's backend scheduler (the calendar backend emits the same payload
// shape alarms use, namespaced to this plugin), announce via the TTS service,
// drive the avatar (surprised + nod), and show a stop/snooze banner.
//
// Graceful degradation: with the voice plugin absent, ctx.services.tts()
// resolves to a silent provider — reminders still show visually.

import { useCallback, useEffect, useRef, useState } from "react";
import type { PluginContext } from "../../sdk/js/@companion/plugin";
import { injectStyles } from "./styles";

const SNOOZE_MS = 5 * 60_000;

type RingingEvent = { id: string; label: string; message: string; loop: boolean; startedAt: number };

type FiredPayload = {
  id: string;
  label: string;
  message: string;
  loop_sound: boolean;
};

export function ReminderBanner({ ctx }: { ctx: PluginContext }) {
  injectStyles();
  const [event, setEvent] = useState<RingingEvent | null>(null);
  const eventRef = useRef<RingingEvent | null>(null);
  eventRef.current = event;
  const snoozeTimerRef = useRef<number | null>(null);

  const fire = useCallback(
    (a: RingingEvent) => {
      const tts = ctx.services.tts();
      const stt = ctx.services.stt();
      tts.stop(); // cut any in-flight announcement first
      if (snoozeTimerRef.current !== null) {
        window.clearTimeout(snoozeTimerRef.current);
        snoozeTimerRef.current = null;
      }
      ctx.avatar.triggerMotion("nod");
      ctx.avatar.setEmotion("surprised");
      void stt?.setSuppress(true);
      // Tag each pass so a loop iteration can't clear a newer event's banner.
      const ringing: RingingEvent = { ...a, startedAt: Date.now() };
      eventRef.current = ringing;
      setEvent(ringing);
      const onEnd = () => {
        if (eventRef.current?.startedAt === ringing.startedAt) {
          eventRef.current = null;
          setEvent(null);
        }
        window.setTimeout(() => void stt?.setSuppress(false), 700);
      };
      const onViseme = (name: string, weight?: number) => ctx.avatar.setViseme(name, weight);
      const text = [a.label, a.message].filter(Boolean).join(". ") || "You have a calendar event.";
      if (ringing.loop) {
        void tts.speakLoop(text, {}, onViseme, onEnd);
      } else {
        void tts.speak(text, {}, onViseme, onEnd);
      }
    },
    [ctx],
  );

  useEffect(() => {
    return ctx.events.on("alarm-fired", (payload) => {
      const f = payload as Partial<FiredPayload> | null;
      if (!f || typeof f !== "object") return;
      fire({
        id: String(f.id ?? ""),
        label: String(f.label ?? ""),
        message: String(f.message ?? ""),
        loop: !!f.loop_sound,
        startedAt: 0,
      });
    });
  }, [ctx, fire]);

  // Halt announcement and clear state.
  const stopEventSpeech = useCallback(() => {
    if (snoozeTimerRef.current !== null) {
      window.clearTimeout(snoozeTimerRef.current);
      snoozeTimerRef.current = null;
    }
    eventRef.current = null;
    setEvent(null);
    ctx.services.tts().stop();
    void ctx.services.stt()?.setSuppress(false);
    ctx.avatar.setEmotion("neutral");
  }, [ctx]);

  // Silence now, repeat in 5 minutes. Keeps the original loop flag so a
  // looping reminder resumes looping.
  const snoozeEvent = useCallback(() => {
    const current = eventRef.current;
    stopEventSpeech();
    if (!current) return;
    snoozeTimerRef.current = window.setTimeout(() => {
      snoozeTimerRef.current = null;
      fire({ ...current, startedAt: 0 });
    }, SNOOZE_MS);
  }, [ctx, fire, stopEventSpeech]);

  if (!event) return null;
  const name = event.label.trim() || "Calendar event";
  return (
    <div className="reminder-banner" role="alert">
      <span className="reminder-banner-icon" aria-hidden="true">
        📅
      </span>
      <span className="reminder-banner-text">
        <span className="reminder-banner-title">{name}</span>
        <span className="reminder-banner-sub">
          {event.loop ? "Reminder — stop or snooze" : "Snooze 5 min or stop"}
        </span>
      </span>
      <span className="reminder-banner-actions">
        <button
          type="button"
          className="reminder-banner-btn reminder-banner-btn-snooze"
          onClick={snoozeEvent}
          aria-label={`Snooze ${name} for 5 minutes`}
        >
          Snooze 5m
        </button>
        <button
          type="button"
          className="reminder-banner-btn reminder-banner-btn-stop"
          onClick={stopEventSpeech}
          aria-label={`Stop ${name}`}
        >
          Stop
        </button>
      </span>
    </div>
  );
}
