// ConfirmModal (copied from the host's ui/molecules/ConfirmModal.tsx).
// Despite being used across the settings page, it is not part of the shared
// ctx.ui kit, so it lives in the plugin and takes Button via the `ui` prop.

import { useEffect } from "react";
// Portal via the local host-compat shim: the host shares only
// react-dom/client, whose React 19 entry no longer carries createPortal.
import { createPortal } from "./vendor/react-dom-compat.js";
import type { PluginContext } from "../../../sdk/js/@companion/plugin";

type Props = {
  ui: PluginContext["ui"];
  title: string;
  message: string;
  confirmLabel?: string;
  busy?: boolean;
  onConfirm: () => void;
  onClose: () => void;
};

export function ConfirmModal({ ui, title, message, confirmLabel = "Delete", busy, onConfirm, onClose }: Props) {
  const { Button } = ui;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, busy]);

  return createPortal(
    <div className="modal-overlay" onMouseDown={(e) => { if (e.target === e.currentTarget && !busy) onClose(); }}>
      <div role="dialog" aria-modal="true" aria-label={title} className="modal-card">
        <div className="modal-head">
          <div className="modal-title">{title}</div>
          <button
            type="button"
            className="modal-x"
            aria-label="Close"
            disabled={busy}
            onClick={onClose}
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round"><path d="M18 6 6 18M6 6l12 12" /></svg>
          </button>
        </div>
        <div className="modal-msg">{message}</div>
        <div className="modal-actions">
          <Button variant="secondary" onClick={onClose} disabled={busy} autoFocus>
            Cancel
          </Button>
          <Button variant="danger" onClick={onConfirm} disabled={busy} className={busy ? "is-busy" : ""}>
            {confirmLabel}
          </Button>
        </div>
      </div>
    </div>,
    document.body,
  );
}
