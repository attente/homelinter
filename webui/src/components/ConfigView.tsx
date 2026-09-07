import { useEffect, useState } from 'react';
import { api, resolveApiError } from '../api';

interface ConfigViewProps {
  onClose: () => void;
  pushToast: (msg: string) => void;
}

export function ConfigView(props: ConfigViewProps) {
  const { onClose, pushToast } = props;
  const [text, setText] = useState('');
  const [saved, setSaved] = useState<Record<string, unknown> | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    api.config()
      .then((cfg) => {
        if (!cancelled) {
          setText(JSON.stringify(cfg, null, 2));
          setSaved(cfg);
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) pushToast(`cannot load config: ${resolveApiError(e)}`);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const parse = (): Record<string, unknown> | null => {
    try {
      return JSON.parse(text) as Record<string, unknown>;
    } catch {
      pushToast('config is not valid JSON');
      return null;
    }
  };

  const onValidate = async () => {
    const doc = parse();
    if (!doc) return;
    setBusy(true);
    try {
      const res = await api.configValidate(doc);
      if (res.valid) {
        pushToast('config validates cleanly');
      } else {
        pushToast(`config rejected: ${res.rejected.map((r) => r.reason).join('; ')}`);
      }
    } catch (e: unknown) {
      pushToast(`validation failed: ${resolveApiError(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const onSave = async () => {
    const doc = parse();
    if (!doc) return;
    setBusy(true);
    try {
      const res = await api.configValidate(doc);
      if (!res.valid) {
        pushToast(`not saved: ${res.rejected.map((r) => r.reason).join('; ')}`);
        return;
      }
      const effective = await api.configPut(doc);
      setSaved(effective);
      pushToast('config saved; applies to the next scan');
    } catch (e: unknown) {
      pushToast(`save failed: ${resolveApiError(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const onClearCache = async () => {
    setBusy(true);
    try {
      await api.cacheClear();
      pushToast('metadata cache cleared');
    } catch (e: unknown) {
      pushToast(`cache clear failed: ${resolveApiError(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className="config-view">
      <div className="toolbar">
        <h2>Configuration</h2>
        <div className="grow" />
        <button type="button" onClick={onValidate} disabled={busy}>validate</button>
        <button type="button" onClick={onSave} disabled={busy}>save</button>
        <button type="button" onClick={onClearCache} disabled={busy}>clear cache</button>
        <button type="button" onClick={onClose}>close</button>
      </div>
      {saved && (
        <p className="note">
          effective config file: {String(saved['config_path'] ?? 'default')}
        </p>
      )}
      <p className="note">
        Changes apply to the next scan: existing suggestions keep the settings they were
        scanned with, and an edit during a scan or execution never changes the running work.
      </p>
      <textarea
        className="config-text"
        value={text}
        onChange={(e) => setText(e.target.value)}
        spellCheck={false}
      />
    </main>
  );
}