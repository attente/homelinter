import { useCallback, useEffect, useRef, useState } from "react";
import { api, resolveApiError } from "../api";

type Json = Record<string, unknown>;

interface ConfigViewProps {
  /**
   * Hash of the service's live configuration. Any change (from any client)
   * refetches the document, so this view always mirrors the service.
   */
  configHash: string | null;
  onClose: () => void;
  pushToast: (msg: string) => void;
}

/// Live mirror of the service's effective configuration. There are no
/// reload or edit actions: the document is refetched whenever the service's
/// config hash changes, so what is shown matches the service regardless of
/// which client changed it (the scan setup page persists changes to the
/// service as they are made; the config file can be edited directly).
export function ConfigView(props: ConfigViewProps) {
  const { configHash, onClose, pushToast } = props;
  const [doc, setDoc] = useState<Json | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);

  const load = useCallback(async () => {
    try {
      const d = await api.config();
      setDoc(d);
    } catch (e: unknown) {
      pushToast(`cannot load config: ${resolveApiError(e)}`);
    }
  }, [pushToast]);

  useEffect(() => {
    void load();
  }, [load]);

  // Mirror: refetch whenever the live hash moves (config.updated event or
  // the 2s session poll). The ref tracks the previously seen hash so the
  // mount load above is not repeated.
  const seenHash = useRef<string | null>(null);
  useEffect(() => {
    const previous = seenHash.current;
    seenHash.current = configHash;
    if (previous !== null && configHash !== null && previous !== configHash)
      void load();
  }, [configHash, load]);

  const onClearCache = async () => {
    setConfirmClear(false);
    try {
      await api.cacheClear();
      pushToast("metadata cache cleared");
    } catch (e: unknown) {
      pushToast(`cache clear failed: ${resolveApiError(e)}`);
    }
  };

  const configPath =
    doc && typeof doc["config_path"] === "string"
      ? String(doc["config_path"])
      : null;

  return (
    <main className="config-view">
      <div className="toolbar">
        <h2>Configuration</h2>
        <div className="grow" />
        <button type="button" onClick={() => setConfirmClear(true)}>
          clear cache
        </button>
        <button type="button" onClick={onClose}>
          close
        </button>
      </div>
      <p className="note">
        This is the service's live effective configuration; it refreshes
        automatically whenever the configuration changes, in any client.
        Settings are edited on the scan setup page (saved to the service as they
        change) or directly in the config file. A key set to null there resets
        to its built-in default. Changes apply to the next scan; a scan in
        progress keeps the configuration snapshot it started under.
      </p>
      {configPath !== null && <p className="note">config file: {configPath}</p>}
      <pre className="config-doc">
        {doc === null ? "loading..." : JSON.stringify(doc, null, 2)}
      </pre>
      <p className="hint">
        Per-kind description overrides ([description.photo] and friends) are
        visible in the document above. API keys are write-only and never shown:
        they are set in the config file or on the scan setup page.
      </p>
      {confirmClear && (
        <div
          className="modal-backdrop"
          role="presentation"
          onClick={() => setConfirmClear(false)}
        >
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            onClick={(e) => e.stopPropagation()}
          >
            <h2>Clear the metadata cache?</h2>
            <p>
              This deletes all cached metadata. The next scan has to re-extract
              everything from scratch, which is slower and may re-run external
              tools. This cannot be undone.
            </p>
            <div className="modal-actions">
              <button
                type="button"
                autoFocus
                onClick={() => setConfirmClear(false)}
              >
                cancel
              </button>
              <button
                type="button"
                className="danger"
                onClick={() => void onClearCache()}
              >
                clear cache
              </button>
            </div>
          </div>
        </div>
      )}
    </main>
  );
}
