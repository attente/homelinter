import { useEffect, useState } from 'react';
import { api, ApiError, resolveApiError } from '../api';

type Json = Record<string, unknown>;

// The nine asset kinds (crates/model/src/asset_kind.rs).
const ASSET_KINDS = [
  'document',
  'photo',
  'video',
  'audio',
  'repository',
  'archive',
  'data',
  'executable',
  'other',
] as const;

const GEOCODER_SERVICES = ['nominatim', 'photon', 'google', 'mapbox', 'custom'];
const LLM_SERVICES = ['ollama', 'openrouter', 'custom'];

// Document formats the extraction form always offers, with the runtime
// default commands (defaults.toml) as placeholders.
const EXTRACTION_DEFAULTS: Array<[string, string]> = [
  ['pdf', 'pdftotext -layout {path} -'],
  ['docx', 'pandoc {path} -t plain'],
  ['odt', 'pandoc {path} -t plain'],
  ['epub', 'pandoc {path} -t plain'],
];

// The form's editable state; every value is a string/bool so inputs stay
// simple. Empty optional strings mean "unset" and are persisted as JSON
// nulls, which DELETE the key from the merged config file so the default
// applies again.
interface FormState {
  targetPaths: Record<string, string>;
  excludePaths: string;
  geocoderEnabled: boolean;
  geocoderService: string;
  geocoderEndpoint: string;
  geocoderApiKey: string;
  geocoderClearKey: boolean;
  geocoderLanguage: string;
  geocoderContact: string;
  geocoderRate: string;
  descEnabled: boolean;
  descService: string;
  descEndpoint: string;
  descModel: string;
  descApiKey: string;
  descClearKey: boolean;
  descMaxWords: string;
  descRate: string;
  largeThreshold: string;
  providerTimeout: string;
  toolTimeout: string;
  toolStdoutMax: string;
  cacheEnabled: boolean;
  extraction: Record<string, string>;
  extractionDisabled: Record<string, boolean>;
}

function str(v: unknown): string {
  return typeof v === 'string' ? v : '';
}

function num(v: unknown): string {
  return typeof v === 'number' ? String(v) : '';
}

function obj(v: unknown): Json {
  return v !== null && typeof v === 'object' ? (v as Json) : {};
}

function formFromDoc(doc: Json): FormState {
  const geo = obj(doc.geocoder);
  const desc = obj(doc.description);
  const dup = obj(doc.duplication);
  const scan = obj(doc.scan);
  const limits = obj(doc.limits);
  const cache = obj(doc.cache);
  const extraction = obj(doc.document_extraction) as Record<string, unknown>;
  const configuredPaths = obj(doc.target_paths) as Record<string, unknown>;
  const targetPaths: Record<string, string> = {};
  for (const kind of ASSET_KINDS) {
    targetPaths[kind] = str(configuredPaths[kind]);
  }
  const formats = Array.from(
    new Set([...EXTRACTION_DEFAULTS.map(([k]) => k), ...Object.keys(extraction)]),
  );
  const extractionText: Record<string, string> = {};
  const extractionDisabled: Record<string, boolean> = {};
  for (const f of formats) {
    const argv = extraction[f];
    if (Array.isArray(argv) && argv.length === 0) {
      extractionDisabled[f] = true;
      extractionText[f] = '';
    } else {
      extractionText[f] = Array.isArray(argv)
        ? argv.map(String).join(' ')
        : '';
      extractionDisabled[f] = false;
    }
  }
  const exclude = scan.exclude_paths;
  return {
    targetPaths,
    excludePaths: Array.isArray(exclude) ? exclude.map(String).join('\n') : '',
    geocoderEnabled: geo.enabled === true,
    geocoderService: str(geo.service) || 'nominatim',
    geocoderEndpoint: str(geo.endpoint),
    geocoderApiKey: '',
    geocoderClearKey: false,
    geocoderLanguage: str(geo.language),
    geocoderContact: str(geo.user_agent_contact),
    geocoderRate: num(geo.rate_limit_ms),
    descEnabled: desc.enabled === true,
    descService: str(desc.service) || 'ollama',
    descEndpoint: str(desc.endpoint) || 'http://localhost:11434',
    descModel: str(desc.model),
    descApiKey: '',
    descClearKey: false,
    descMaxWords: num(desc.max_words),
    descRate: num(desc.rate_limit_ms),
    largeThreshold: num(dup.large_file_threshold),
    providerTimeout: num(limits.provider_timeout_seconds),
    toolTimeout: num(limits.tool_timeout_seconds),
    toolStdoutMax: num(limits.tool_stdout_max_bytes),
    cacheEnabled: cache.enabled !== false,
    extraction: extractionText,
    extractionDisabled,
  };
}

/// Build the partial PUT /config document from the form: only fields that
/// differ from the loaded effective config are included, so saving never
/// pins untouched values into the config file. Empty optional values are
/// sent as null (which deletes the persisted key).
function buildDoc(form: FormState, doc: Json): Json {
  const out: Json = {};

  const paths = obj(doc.target_paths);
  const tp: Record<string, unknown> = {};
  for (const kind of ASSET_KINDS) {
    const v = (form.targetPaths[kind] ?? '').trim();
    const orig = typeof paths[kind] === 'string' ? (paths[kind] as string) : null;
    if (v.length === 0) {
      if (orig !== null) tp[kind] = null;
    } else if (v !== orig) {
      tp[kind] = v;
    }
  }
  if (Object.keys(tp).length > 0) out.target_paths = tp;

  const patterns = form.excludePaths
    .split('\n')
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  const curExclude = Array.isArray(obj(doc.scan).exclude_paths)
    ? (obj(doc.scan).exclude_paths as unknown[]).map(String)
    : [];
  if (JSON.stringify(patterns) !== JSON.stringify(curExclude)) {
    out.scan = { exclude_paths: patterns };
  }

  const geo = obj(doc.geocoder);
  const geoOut: Json = {};
  if (form.geocoderEnabled !== (geo.enabled === true)) {
    geoOut.enabled = form.geocoderEnabled;
  }
  if (form.geocoderService !== str(geo.service)) {
    geoOut.service = form.geocoderService;
  }
  if (form.geocoderEndpoint.trim() !== str(geo.endpoint)) {
    const v = form.geocoderEndpoint.trim();
    geoOut.endpoint = v.length > 0 ? v : null;
  }
  if (form.geocoderClearKey) {
    geoOut.api_key = null;
  } else if (form.geocoderApiKey.length > 0) {
    geoOut.api_key = form.geocoderApiKey;
  }
  if (form.geocoderLanguage.trim() !== str(geo.language)) {
    const v = form.geocoderLanguage.trim();
    geoOut.language = v.length > 0 ? v : null;
  }
  if (form.geocoderContact.trim() !== str(geo.user_agent_contact)) {
    const v = form.geocoderContact.trim();
    geoOut.user_agent_contact = v.length > 0 ? v : null;
  }
  if (form.geocoderRate.trim() !== num(geo.rate_limit_ms)) {
    const n = Number(form.geocoderRate);
    geoOut.rate_limit_ms = form.geocoderRate.trim() === '' || Number.isNaN(n) ? null : n;
  }
  if (Object.keys(geoOut).length > 0) out.geocoder = geoOut;

  const desc = obj(doc.description);
  const descOut: Json = {};
  if (form.descEnabled !== (desc.enabled === true)) {
    descOut.enabled = form.descEnabled;
  }
  if (form.descService !== str(desc.service)) {
    descOut.service = form.descService;
  }
  if (form.descEndpoint.trim() !== str(desc.endpoint)) {
    const v = form.descEndpoint.trim();
    descOut.endpoint = v.length > 0 ? v : null;
  }
  if (form.descModel.trim() !== str(desc.model)) {
    const v = form.descModel.trim();
    descOut.model = v.length > 0 ? v : null;
  }
  if (form.descClearKey) {
    descOut.api_key = null;
  } else if (form.descApiKey.length > 0) {
    descOut.api_key = form.descApiKey;
  }
  if (form.descMaxWords.trim() !== num(desc.max_words)) {
    const n = Number(form.descMaxWords);
    descOut.max_words = form.descMaxWords.trim() === '' || Number.isNaN(n) ? null : n;
  }
  if (form.descRate.trim() !== num(desc.rate_limit_ms)) {
    const n = Number(form.descRate);
    descOut.rate_limit_ms = form.descRate.trim() === '' || Number.isNaN(n) ? null : n;
  }
  if (Object.keys(descOut).length > 0) out.description = descOut;

  const dup = obj(doc.duplication);
  if (form.largeThreshold.trim() !== num(dup.large_file_threshold)) {
    const n = Number(form.largeThreshold);
    out.duplication = {
      large_file_threshold:
        form.largeThreshold.trim() === '' || Number.isNaN(n) ? null : n,
    };
  }

  const limits = obj(doc.limits);
  const limOut: Json = {};
  const limit = (loaded: unknown, value: string): unknown => {
    if (value.trim() === num(loaded)) return undefined;
    const n = Number(value);
    return value.trim() === '' || Number.isNaN(n) ? null : n;
  };
  const p = limit(limits.provider_timeout_seconds, form.providerTimeout);
  if (p !== undefined) limOut.provider_timeout_seconds = p;
  const t = limit(limits.tool_timeout_seconds, form.toolTimeout);
  if (t !== undefined) limOut.tool_timeout_seconds = t;
  const s = limit(limits.tool_stdout_max_bytes, form.toolStdoutMax);
  if (s !== undefined) limOut.tool_stdout_max_bytes = s;
  if (Object.keys(limOut).length > 0) out.limits = limOut;

  const cache = obj(doc.cache);
  if (form.cacheEnabled !== (cache.enabled !== false)) {
    out.cache = { enabled: form.cacheEnabled };
  }

  const extraction = obj(doc.document_extraction) as Record<string, unknown>;
  const exOut: Record<string, unknown> = {};
  for (const [format] of EXTRACTION_DEFAULTS) {
    const initial = extraction[format];
    const initialArr = Array.isArray(initial) ? initial.map(String) : null;
    if (form.extractionDisabled[format]) {
      if (initialArr === null || initialArr.length > 0) exOut[format] = [];
      continue;
    }
    const argv = form.extraction[format].trim().split(/\s+/).filter((a) => a.length > 0);
    if (argv.length === 0) {
      if (initialArr !== null) exOut[format] = null;
      continue;
    }
    if (initialArr === null || argv.join(' ') !== initialArr.join(' ')) {
      exOut[format] = argv;
    }
  }
  if (Object.keys(exOut).length > 0) out.document_extraction = exOut;

  return out;
}

function describeConfigError(e: unknown): string {
  if (e instanceof ApiError && e.details) {
    const fields = Object.entries(e.details).map(([k, v]) => `${k}: ${String(v)}`);
    if (fields.length > 0) return `${e.message} (${fields.join('; ')})`;
  }
  return resolveApiError(e);
}

interface ConfigViewProps {
  onClose: () => void;
  pushToast: (msg: string) => void;
}

export function ConfigView(props: ConfigViewProps) {
  const { onClose, pushToast } = props;
  const [doc, setDoc] = useState<Json | null>(null);
  const [form, setForm] = useState<FormState | null>(null);
  const [busy, setBusy] = useState(false);

  const load = () => {
    api
      .config()
      .then((cfg) => {
        setDoc(cfg);
        setForm(formFromDoc(cfg));
      })
      .catch((e: unknown) => pushToast(`cannot load config: ${resolveApiError(e)}`));
  };

  useEffect(load, []); // eslint-disable-line react-hooks/exhaustive-deps

  const patch = (p: Partial<FormState>) => setForm((f) => (f ? { ...f, ...p } : f));

  const onSave = async () => {
    if (!form || !doc) return;
    const body = buildDoc(form, doc);
    if (Object.keys(body).length === 0) {
      pushToast('no changes to save');
      return;
    }
    setBusy(true);
    try {
      const effective = await api.configPut(body);
      setDoc(effective);
      setForm(formFromDoc(effective));
      pushToast('settings saved; applies to the next scan');
    } catch (e: unknown) {
      pushToast(`not saved: ${describeConfigError(e)}`);
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

  const pathDefaults = obj(doc?.target_path_defaults) as Record<string, string>;

  return (
    <main className="config-view">
      <div className="toolbar">
        <h2>Settings</h2>
        <div className="grow" />
        <button type="button" onClick={load} disabled={busy}>reload</button>
        <button type="button" onClick={onSave} disabled={busy || !form}>save</button>
        <button type="button" onClick={onClearCache} disabled={busy}>clear cache</button>
        <button type="button" onClick={onClose}>close</button>
      </div>
      {doc !== null && typeof doc['config_path'] !== 'undefined' && (
        <p className="note">effective config file: {String(doc['config_path'] ?? 'default')}</p>
      )}
      <p className="note">
        Changes apply to the next scan: existing suggestions keep the settings they
        were scanned with, and an edit during a scan or execution never changes the
        running work. API keys are write-only and are never returned by the service.
      </p>
      {form && (
        <div className="settings">
          <section>
            <h3>Destination paths</h3>
            <p className="hint">
              Where each asset kind is moved: a format string with at least one
              placeholder, relative to home or absolute. Empty uses the built-in
              default shown as the placeholder.
            </p>
            <div className="field-grid">
              {ASSET_KINDS.map((kind) => (
                <label className="field" key={kind}>
                  <span>{kind}</span>
                  <input
                    type="text"
                    value={form.targetPaths[kind] ?? ''}
                    placeholder={pathDefaults[kind] ?? ''}
                    onChange={(e) =>
                      patch({ targetPaths: { ...form.targetPaths, [kind]: e.target.value } })
                    }
                  />
                </label>
              ))}
            </div>
          </section>

          <section>
            <h3>Scan exclusions</h3>
            <p className="hint">
              One glob per line, applied to primary discovery. A pattern with a
              separator matches from the scan root; without one it matches a file
              name at any depth.
            </p>
            <textarea
              rows={4}
              value={form.excludePaths}
              onChange={(e) => patch({ excludePaths: e.target.value })}
              spellCheck={false}
            />
          </section>

          <section>
            <h3>Geocoder (%l)</h3>
            <div className="field-grid">
              <label className="field check">
                <input
                  type="checkbox"
                  checked={form.geocoderEnabled}
                  onChange={(e) => patch({ geocoderEnabled: e.target.checked })}
                />
                <span>enabled</span>
              </label>
              <label className="field">
                <span>service</span>
                <select
                  value={form.geocoderService}
                  onChange={(e) => patch({ geocoderService: e.target.value })}
                >
                  {GEOCODER_SERVICES.map((s) => (
                    <option key={s} value={s}>{s}</option>
                  ))}
                </select>
              </label>
              <label className="field">
                <span>endpoint</span>
                <input
                  type="text"
                  value={form.geocoderEndpoint}
                  onChange={(e) => patch({ geocoderEndpoint: e.target.value })}
                />
              </label>
              <label className="field">
                <span>api key {form.geocoderClearKey ? '(will be cleared)' : '(stored one kept when empty)'}</span>
                <input
                  type="password"
                  value={form.geocoderApiKey}
                  disabled={form.geocoderClearKey}
                  onChange={(e) => patch({ geocoderApiKey: e.target.value })}
                />
              </label>
              <label className="field check">
                <input
                  type="checkbox"
                  checked={form.geocoderClearKey}
                  onChange={(e) => patch({ geocoderClearKey: e.target.checked })}
                />
                <span>clear stored api key</span>
              </label>
              <label className="field">
                <span>language (ISO 639-1)</span>
                <input
                  type="text"
                  value={form.geocoderLanguage}
                  onChange={(e) => patch({ geocoderLanguage: e.target.value })}
                />
              </label>
              <label className="field">
                <span>user-agent contact</span>
                <input
                  type="text"
                  value={form.geocoderContact}
                  onChange={(e) => patch({ geocoderContact: e.target.value })}
                />
              </label>
              <label className="field">
                <span>rate limit (ms)</span>
                <input
                  type="number"
                  min={0}
                  value={form.geocoderRate}
                  onChange={(e) => patch({ geocoderRate: e.target.value })}
                />
              </label>
            </div>
          </section>

          <section>
            <h3>Description model (%s)</h3>
            <div className="field-grid">
              <label className="field check">
                <input
                  type="checkbox"
                  checked={form.descEnabled}
                  onChange={(e) => patch({ descEnabled: e.target.checked })}
                />
                <span>enabled</span>
              </label>
              <label className="field">
                <span>service</span>
                <select
                  value={form.descService}
                  onChange={(e) => patch({ descService: e.target.value })}
                >
                  {LLM_SERVICES.map((s) => (
                    <option key={s} value={s}>{s}</option>
                  ))}
                </select>
              </label>
              <label className="field">
                <span>endpoint</span>
                <input
                  type="text"
                  value={form.descEndpoint}
                  onChange={(e) => patch({ descEndpoint: e.target.value })}
                />
              </label>
              <label className="field">
                <span>model</span>
                <input
                  type="text"
                  value={form.descModel}
                  onChange={(e) => patch({ descModel: e.target.value })}
                />
              </label>
              <label className="field">
                <span>api key {form.descClearKey ? '(will be cleared)' : '(stored one kept when empty)'}</span>
                <input
                  type="password"
                  value={form.descApiKey}
                  disabled={form.descClearKey}
                  onChange={(e) => patch({ descApiKey: e.target.value })}
                />
              </label>
              <label className="field check">
                <input
                  type="checkbox"
                  checked={form.descClearKey}
                  onChange={(e) => patch({ descClearKey: e.target.checked })}
                />
                <span>clear stored api key</span>
              </label>
              <label className="field">
                <span>max words</span>
                <input
                  type="number"
                  min={1}
                  value={form.descMaxWords}
                  onChange={(e) => patch({ descMaxWords: e.target.value })}
                />
              </label>
              <label className="field">
                <span>rate limit (ms)</span>
                <input
                  type="number"
                  min={0}
                  value={form.descRate}
                  onChange={(e) => patch({ descRate: e.target.value })}
                />
              </label>
            </div>
          </section>

          <section>
            <h3>Duplication, limits & cache</h3>
            <div className="field-grid">
              <label className="field">
                <span>large file threshold (bytes)</span>
                <input
                  type="number"
                  min={0}
                  value={form.largeThreshold}
                  onChange={(e) => patch({ largeThreshold: e.target.value })}
                />
              </label>
              <label className="field">
                <span>provider timeout (s)</span>
                <input
                  type="number"
                  min={1}
                  value={form.providerTimeout}
                  onChange={(e) => patch({ providerTimeout: e.target.value })}
                />
              </label>
              <label className="field">
                <span>tool timeout (s)</span>
                <input
                  type="number"
                  min={1}
                  value={form.toolTimeout}
                  onChange={(e) => patch({ toolTimeout: e.target.value })}
                />
              </label>
              <label className="field">
                <span>tool stdout max bytes</span>
                <input
                  type="number"
                  min={1}
                  value={form.toolStdoutMax}
                  onChange={(e) => patch({ toolStdoutMax: e.target.value })}
                />
              </label>
              <label className="field check">
                <input
                  type="checkbox"
                  checked={form.cacheEnabled}
                  onChange={(e) => patch({ cacheEnabled: e.target.checked })}
                />
                <span>metadata cache enabled</span>
              </label>
            </div>
          </section>

          <section>
            <h3>Document text extraction</h3>
            <p className="hint">
              Command run per format before description generation; arguments are
              split on spaces and {'{path}'} marks the input file. Empty uses the
              default tool selection; the disable checkbox sends an empty argv,
              which turns extraction off for that format.
            </p>
            <div className="field-grid">
              {EXTRACTION_DEFAULTS.map(([format, def]) => (
                <label className="field" key={format}>
                  <span>{format}</span>
                  <input
                    type="text"
                    value={form.extraction[format] ?? ''}
                    placeholder={def}
                    disabled={form.extractionDisabled[format] === true}
                    onChange={(e) =>
                      patch({ extraction: { ...form.extraction, [format]: e.target.value } })
                    }
                  />
                </label>
              ))}
            </div>
            <div className="field-grid">
              {EXTRACTION_DEFAULTS.map(([format]) => (
                <label className="field check" key={format}>
                  <input
                    type="checkbox"
                    checked={form.extractionDisabled[format] ?? false}
                    onChange={(e) =>
                      patch({
                        extractionDisabled: {
                          ...form.extractionDisabled,
                          [format]: e.target.checked,
                        },
                      })
                    }
                  />
                  <span>disable {format} extraction</span>
                </label>
              ))}
            </div>
          </section>

          <details className="config-json">
            <summary>effective configuration document (read-only)</summary>
            <pre>{JSON.stringify(doc, null, 2)}</pre>
            <p className="hint">
              Per-kind description overrides ([description.photo] and friends) and
              custom extraction format keys are visible here; edit them in the
              config file itself.
            </p>
          </details>
        </div>
      )}
    </main>
  );
}
