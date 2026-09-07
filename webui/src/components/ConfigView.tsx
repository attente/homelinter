import { useEffect, useState } from 'react';
import { api, resolveApiError } from '../api';

type Json = Record<string, unknown>;

interface ConfigViewProps {
  onClose: () => void;
  pushToast: (msg: string) => void;
}

/// Read-only reference view of the effective configuration. The effective
/// configuration cannot be changed for the current scan, so this page only
/// displays it: durable settings live in the config file (edit it directly
/// and reload), and per-scan overrides live in the scan dialog.
export function ConfigView(props: ConfigViewProps) {
  const { onClose, pushToast } = props;
  const [doc, setDoc] = useState<Json | null>(null);

  const load = () => {
    api
      .config()
      .then(setDoc)
      .catch((e: unknown) => pushToast(`cannot load config: ${resolveApiError(e)}`));
  };

  useEffect(load, []); // eslint-disable-line react-hooks/exhaustive-deps

  const onClearCache = async () => {
    try {
      await api.cacheClear();
      pushToast('metadata cache cleared');
    } catch (e: unknown) {
      pushToast(`cache clear failed: ${resolveApiError(e)}`);
    }
  };

  const configPath =
    doc && typeof doc['config_path'] === 'string' ? String(doc['config_path']) : null;

  return (
    <main className="config-view">
      <div className="toolbar">
        <h2>Configuration</h2>
        <div className="grow" />
        <button type="button" onClick={load}>reload</button>
        <button type="button" onClick={onClearCache}>clear cache</button>
        <button type="button" onClick={onClose}>close</button>
      </div>
      <p className="note">
        Read-only: this is the effective configuration, and it cannot be changed
        for the current scan. Durable settings live in the config file (edit it
        directly, then reload); per-scan overrides - including providers,
        exclusions, and extraction commands - are set in the scan dialog and
        apply to the next scan only.
      </p>
      {configPath !== null && <p className="note">config file: {configPath}</p>}
      <pre className="config-doc">
        {doc === null ? 'loading...' : JSON.stringify(doc, null, 2)}
      </pre>
      <p className="hint">
        Per-kind description overrides ([description.photo] and friends) are
        visible in the document above. API keys are never shown: they are set
        in the config file or per scan.
      </p>
    </main>
  );
}
