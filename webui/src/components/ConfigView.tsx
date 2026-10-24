import { useCallback, useEffect, useRef, useState } from "react";
import { api, resolveApiError } from "../api";
import { useEscapeClose } from "../lib/modal";
import type { CacheList } from "../types";
import { CloseIcon } from "./CloseIcon";

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

/// Live mirror of the service's effective configuration. The document has no
/// reload or edit actions of its own: it is refetched whenever the service's
/// config hash changes, so what is shown matches the service regardless of
/// which client changed it (the scan setup page persists changes to the
/// service as they are made; the config file can be edited directly). The
/// only actions here are opening the metadata cache viewer and closing; the
/// cache viewer modal itself offers refresh and clearing.
export function ConfigView(props: ConfigViewProps) {
  const { configHash, onClose, pushToast } = props;
  const [doc, setDoc] = useState<Json | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [cacheOpen, setCacheOpen] = useState(false);
  const [cache, setCache] = useState<CacheList | null>(null);
  const [cacheKind, setCacheKind] = useState("");
  const [cacheLoading, setCacheLoading] = useState(false);
  // Escape dismisses the dialogs like their close buttons: the screen
  // itself is registered first (it is the bottom of the stack) and the
  // modals open on top, so with both open (the viewer under its
  // clear-cache confirmation) Escape closes the topmost first.
  useEscapeClose(true, onClose);
  useEscapeClose(cacheOpen, () => setCacheOpen(false));
  useEscapeClose(confirmClear, () => setConfirmClear(false));

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

  const loadCache = useCallback(async () => {
    setCacheLoading(true);
    try {
      const list = await api.cacheList({
        kind: cacheKind || undefined,
        limit: 200,
      });
      setCache(list);
    } catch (e: unknown) {
      pushToast(`cannot load cache: ${resolveApiError(e)}`);
    } finally {
      setCacheLoading(false);
    }
  }, [cacheKind, pushToast]);

  useEffect(() => {
    if (cacheOpen) void loadCache();
  }, [cacheOpen, loadCache]);

  const onClearCache = async () => {
    setConfirmClear(false);
    try {
      await api.cacheClear();
      pushToast("metadata cache cleared");
      if (cacheOpen) await loadCache();
    } catch (e: unknown) {
      pushToast(`cache clear failed: ${resolveApiError(e)}`);
    }
  };

  const configPath =
    doc && typeof doc["config_path"] === "string"
      ? String(doc["config_path"])
      : null;
  const rawWarnings = doc && typeof doc["warnings"] === "object"
    ? (doc["warnings"] as Record<string, unknown>)
    : {};
  const configWarnings = Object.entries(rawWarnings).map(
    ([field, reason]) => [field, String(reason)] as const,
  );

  return (
    <main className="config-view">
      <div className="toolbar">
        <h2>Configuration</h2>
        <div className="grow" />
        <button type="button" onClick={() => setCacheOpen(true)}>
          view cache
        </button>
        <button
          type="button"
          className="close-btn"
          aria-label="close"
          title="close"
          onClick={onClose}
        >
          <CloseIcon />
        </button>
      </div>
      <p className="note">
        The service's live effective configuration, refreshed whenever any
        client changes it. Edit settings on the scan setup page or in the
        config file; null resets a key to its default. Changes apply to the
        next scan, while a scan in progress keeps its snapshot.
      </p>
      {configWarnings.length > 0 && (
        <div className="config-warnings">
          {configWarnings.map(([field, reason]) => (
            <p key={field} className="note warn">
              <strong>{field}:</strong> {reason}
            </p>
          ))}
        </div>
      )}
      {configPath !== null && <p className="note">config file: {configPath}</p>}
      <pre className="config-doc">
        {doc === null ? "loading..." : JSON.stringify(doc, null, 2)}
      </pre>
      <p className="hint">
        Per-modality description overrides ([description.image] and friends)
        appear above. API keys are write-only; set them in the config file or
        on the scan setup page.
      </p>
      {cacheOpen && (
        <div
          className="modal-backdrop"
          role="presentation"
          onClick={() => setCacheOpen(false)}
        >
          <div
            className="modal cache-modal"
            role="dialog"
            aria-modal="true"
            aria-label="metadata cache"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="toolbar-row">
              <h3>Metadata cache</h3>
              <div className="grow" />
              <label>
                kind{" "}
                <select
                  value={cacheKind}
                  onChange={(e) => setCacheKind(e.target.value)}
                >
                  <option value="">all</option>
                  {(cache?.kind_counts ?? []).map(({ kind, count }) => (
                    <option key={kind} value={kind}>
                      {kind} ({count})
                    </option>
                  ))}
                </select>
              </label>
              <button
                type="button"
                onClick={() => void loadCache()}
                disabled={cacheLoading}
              >
                refresh
              </button>
              <button
                type="button"
                className="caution"
                onClick={() => setConfirmClear(true)}
                disabled={cache === null}
              >
                clear cache
              </button>
              <button
                type="button"
                className="close-btn"
                autoFocus
                aria-label="close"
                title="close"
                onClick={() => setCacheOpen(false)}
              >
                <CloseIcon />
              </button>
            </div>
            <p className="note">
              {cache === null || cacheLoading
                ? "loading..."
                : `${cache.entries.length} of ${cache.total} entries (newest first, up to 200 shown)`}
            </p>
            {cache !== null && cache.entries.length > 0 && (
              <div className="cache-scroll">
                <table className="cache-table">
                  <thead>
                    <tr>
                      <th>created</th>
                      <th>kind</th>
                      <th>provider</th>
                      <th>value</th>
                      <th>input hash</th>
                    </tr>
                  </thead>
                  <tbody>
                    {cache.entries.map((e) => (
                      <tr key={e.cache_key}>
                        <td>{e.created_at}</td>
                        <td>{e.kind}</td>
                        <td>{e.provider}</td>
                        <td className="cache-value">{e.value}</td>
                        <td className="cache-mono">
                          {e.input_hash.slice(0, 16)}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            {cache !== null && !cacheLoading && cache.entries.length === 0 && (
              <p className="empty">no cached entries</p>
            )}
          </div>
        </div>
      )}
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
              Deletes all cached metadata. The next scan re-extracts everything
              from scratch, which is slower and may re-run external tools. This
              cannot be undone.
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
                className="caution"
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
