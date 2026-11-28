// Minimal vendored Tauri bridge (same pattern as the chat plugin's
// src/tauri.ts): plugin bundles cannot resolve @tauri-apps/api, so the raw
// invoke is read off window.__TAURI_INTERNALS__. Used only for the
// host-global `open_url` command, which opens the Google consent screen in
// the default browser. Everything else goes through ctx.*.

interface TauriInternals {
  invoke: (cmd: string, args?: Record<string, unknown>, options?: unknown) => Promise<unknown>;
}

function internals(): TauriInternals | undefined {
  return typeof window !== "undefined"
    ? (window as unknown as { __TAURI_INTERNALS__?: TauriInternals }).__TAURI_INTERNALS__
    : undefined;
}

export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** Raw Tauri command invocation (escape hatch — prefer ctx.* surfaces). */
export function tauriInvoke<T = unknown>(
  cmd: string,
  args: Record<string, unknown> = {},
): Promise<T> {
  const t = internals();
  if (!t || typeof t.invoke !== "function") {
    return Promise.reject(new Error("Tauri IPC unavailable (not running inside companion)"));
  }
  return t.invoke(cmd, args) as Promise<T>;
}
