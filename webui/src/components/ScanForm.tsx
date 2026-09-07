import { useEffect, useState } from 'react';
import { api } from '../api';

type Json = Record<string, unknown>;

export interface ScanFormValues {
  root: string;
  hidden: boolean;
  targetPaths: Record<string, string>;
  /** Partial per-scan config overlay; empty object = inherit everything. */
  config: Record<string, unknown>;
}

// The nine asset kinds with their built-in default destination format
// strings (DESIGN.md "Destination Path Format Strings > Defaults"); the
// defaults double as input placeholders. Empty input = use the configured
// (or built-in) default for that kind.
const KINDS: Array<[string, string]> = [
  ['document', 'Documents/%f'],
  ['photo', 'Pictures/%Y/%m/%f'],
  ['video', 'Videos/%Y/%m/%f'],
  ['audio', 'Music/%f'],
  ['repository', 'Projects/%f'],
  ['archive', 'Archives/%f'],
  ['data', 'Data/%f'],
  ['executable', 'bin/%f'],
  ['other', 'Other/%f'],
];

const GEOCODER_SERVICES = ['nominatim', 'photon', 'google', 'mapbox', 'custom'];
const LLM_SERVICES = ['ollama', 'openrouter', 'custom'];

// Document formats the extraction section always offers, with the runtime
// default commands (defaults.toml) as placeholders.
const EXTRACTION_DEFAULTS: Array<[string, string]> = [
  ['pdf', 'pdftotext -layout {path} -'],
  ['docx', 'pandoc {path} -t plain'],
  ['odt', 'pandoc {path} -t plain'],
  ['epub', 'pandoc {path} -t plain'],
];

// Every override field starts at the effective configuration (fetched on
// mount); the overlay sent with the scan contains only what changed.
// Clearing a field that the config sets resets that key to its built-in
// default for this scan (sent as null, which deletes the key).
interface OverrideState {
  excludePaths: string;
  geoEnabled: boolean;
  geoService: string;
  geoEndpoint: string;
  geoApiKey: string;
  geoLanguage: string;
  geoContact: string;
  geoRate: string;
  descEnabled: boolean;
  descService: string;
  descEndpoint: string;
  descModel: string;
  descApiKey: string;
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

function exclusionsOf(doc: Json | null): string[] {
  const raw = doc === null ? undefined : obj(doc.scan).exclude_paths;
  return Array.isArray(raw) ? raw.map(String) : [];
}

function formFromDoc(doc: Json): OverrideState {
  const geo = obj(doc.geocoder);
  const desc = obj(doc.description);
  const dup = obj(doc.duplication);
  const limits = obj(doc.limits);
  const cache = obj(doc.cache);
  const extraction = obj(doc.document_extraction) as Record<string, unknown>;
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
      extractionDisabled[f] = false;
      extractionText[f] = Array.isArray(argv) ? argv.map(String).join(' ') : '';
    }
  }
  return {
    excludePaths: exclusionsOf(doc).join('\n'),
    geoEnabled: geo.enabled === true,
    geoService: '',
    geoEndpoint: str(geo.endpoint),
    geoApiKey: '',
    geoLanguage: str(geo.language),
    geoContact: str(geo.user_agent_contact),
    geoRate: num(geo.rate_limit_ms),
    descEnabled: desc.enabled === true,
    descService: '',
    descEndpoint: str(desc.endpoint),
    descModel: str(desc.model),
    descApiKey: '',
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

/// Build the per-scan overlay: only fields that differ from the effective
/// configuration document are included, so an untouched form inherits
/// everything. Cleared optional values are sent as null, which deletes the
/// key for this scan so the built-in default applies.
function buildOverlay(form: OverrideState, doc: Json): Json {
  const out: Json = {};

  const patterns = form.excludePaths
    .split('\n')
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  if (JSON.stringify(patterns) !== JSON.stringify(exclusionsOf(doc))) {
    out.scan = { exclude_paths: patterns };
  }

  const effGeo = obj(doc.geocoder);
  const geo: Json = {};
  if (form.geoEnabled !== (effGeo.enabled === true)) {
    geo.enabled = form.geoEnabled;
  }
  if (form.geoService.length > 0) {
    geo.service = form.geoService;
  }
  if (form.geoEndpoint.trim() !== str(effGeo.endpoint)) {
    const v = form.geoEndpoint.trim();
    geo.endpoint = v.length > 0 ? v : null;
  }
  if (form.geoApiKey.length > 0) {
    geo.api_key = form.geoApiKey;
  }
  if (form.geoLanguage.trim() !== str(effGeo.language)) {
    const v = form.geoLanguage.trim();
    geo.language = v.length > 0 ? v : null;
  }
  if (form.geoContact.trim() !== str(effGeo.user_agent_contact)) {
    const v = form.geoContact.trim();
    geo.user_agent_contact = v.length > 0 ? v : null;
  }
  if (form.geoRate.trim() !== num(effGeo.rate_limit_ms)) {
    const n = Number(form.geoRate);
    geo.rate_limit_ms = form.geoRate.trim() === '' || Number.isNaN(n) ? null : n;
  }
  if (Object.keys(geo).length > 0) out.geocoder = geo;

  const effDesc = obj(doc.description);
  const desc: Json = {};
  if (form.descEnabled !== (effDesc.enabled === true)) {
    desc.enabled = form.descEnabled;
  }
  if (form.descService.length > 0) {
    desc.service = form.descService;
  }
  if (form.descEndpoint.trim() !== str(effDesc.endpoint)) {
    const v = form.descEndpoint.trim();
    desc.endpoint = v.length > 0 ? v : null;
  }
  if (form.descModel.trim() !== str(effDesc.model)) {
    const v = form.descModel.trim();
    desc.model = v.length > 0 ? v : null;
  }
  if (form.descApiKey.length > 0) {
    desc.api_key = form.descApiKey;
  }
  if (form.descMaxWords.trim() !== num(effDesc.max_words)) {
    const n = Number(form.descMaxWords);
    desc.max_words = form.descMaxWords.trim() === '' || Number.isNaN(n) ? null : n;
  }
  if (form.descRate.trim() !== num(effDesc.rate_limit_ms)) {
    const n = Number(form.descRate);
    desc.rate_limit_ms = form.descRate.trim() === '' || Number.isNaN(n) ? null : n;
  }
  if (Object.keys(desc).length > 0) out.description = desc;

  const effDup = obj(doc.duplication);
  if (form.largeThreshold.trim() !== num(effDup.large_file_threshold)) {
    const n = Number(form.largeThreshold);
    out.duplication = {
      large_file_threshold:
        form.largeThreshold.trim() === '' || Number.isNaN(n) ? null : n,
    };
  }

  const effLim = obj(doc.limits);
  const lim: Json = {};
  const limit = (loaded: unknown, value: string): unknown => {
    if (value.trim() === num(loaded)) return undefined;
    const n = Number(value);
    return value.trim() === '' || Number.isNaN(n) ? null : n;
  };
  const p = limit(effLim.provider_timeout_seconds, form.providerTimeout);
  if (p !== undefined) lim.provider_timeout_seconds = p;
  const t = limit(effLim.tool_timeout_seconds, form.toolTimeout);
  if (t !== undefined) lim.tool_timeout_seconds = t;
  const s = limit(effLim.tool_stdout_max_bytes, form.toolStdoutMax);
  if (s !== undefined) lim.tool_stdout_max_bytes = s;
  if (Object.keys(lim).length > 0) out.limits = lim;

  if (form.cacheEnabled !== (obj(doc.cache).enabled !== false)) {
    out.cache = { enabled: form.cacheEnabled };
  }

  const effEx = obj(doc.document_extraction) as Record<string, unknown>;
  const ex: Record<string, unknown> = {};
  for (const [format] of EXTRACTION_DEFAULTS) {
    const initial = effEx[format];
    const initialArr = Array.isArray(initial) ? initial.map(String) : null;
    if (form.extractionDisabled[format]) {
      if (initialArr === null || initialArr.length > 0) ex[format] = [];
      continue;
    }
    const argv = form.extraction[format].trim().split(/\s+/).filter((a) => a.length > 0);
    if (argv.length === 0) {
      // Cleared while the config carries a custom or disabled command:
      // reset to the built-in default tool for this scan.
      if (initialArr !== null && initialArr.length > 0) ex[format] = null;
      continue;
    }
    if (initialArr === null || argv.join(' ') !== initialArr.join(' ')) {
      ex[format] = argv;
    }
  }
  if (Object.keys(ex).length > 0) out.document_extraction = ex;

  return out;
}

interface ScanFormProps {
  defaultRoot: string;
  defaultHidden?: boolean;
  defaultTargetPaths?: Record<string, string>;
  hasSuggestions: boolean;
  startDisabled?: boolean;
  onStart: (values: ScanFormValues) => void;
  onResume: () => void;
}

export function ScanForm(props: ScanFormProps) {
  const {
    defaultRoot,
    defaultHidden,
    defaultTargetPaths,
    hasSuggestions,
    startDisabled,
    onStart,
    onResume,
  } = props;
  const [root, setRoot] = useState(defaultRoot);
  const [hidden, setHidden] = useState(defaultHidden ?? false);
  const [paths, setPaths] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {};
    for (const [kind] of KINDS) {
      init[kind] = defaultTargetPaths?.[kind] ?? '';
    }
    return init;
  });
  const [doc, setDoc] = useState<Json | null>(null);
  const [form, setForm] = useState<OverrideState | null>(null);

  useEffect(() => {
    api
      .config()
      .then((c) => {
        setDoc(c);
        setForm(formFromDoc(c));
      })
      .catch(() => undefined);
  }, []);

  const setPath = (kind: string, value: string) =>
    setPaths((prev) => ({ ...prev, [kind]: value }));

  const patch = (p: Partial<OverrideState>) =>
    setForm((f) => (f ? { ...f, ...p } : f));

  const setExtraction = (format: string, value: string) =>
    setForm((f) => (f ? { ...f, extraction: { ...f.extraction, [format]: value } } : f));

  const setExtractionDisabled = (format: string, value: boolean) =>
    setForm((f) =>
      f ? { ...f, extractionDisabled: { ...f.extractionDisabled, [format]: value } } : f,
    );

  const pathDefaults = obj(doc?.target_path_defaults) as Record<string, string>;
  const geoDoc = obj(doc?.geocoder);
  const descDoc = obj(doc?.description);

  const start = () => {
    const targetPaths: Record<string, string> = {};
    for (const [kind] of KINDS) {
      const t = (paths[kind] ?? '').trim();
      if (t.length > 0) targetPaths[kind] = t;
    }
    onStart({
      root: root.trim(),
      hidden,
      targetPaths: paths,
      config: form && doc ? buildOverlay(form, doc) : {},
    });
  };

  return (
    <main className="scan-form">
      <h1>homelinter</h1>
      <p className="subtitle">Duplicate and home cleanup suggestions for your files</p>
      {hasSuggestions && (
        <div className="resume">
          <span>An existing scan session has suggestions ready.</span>
          <button type="button" onClick={onResume}>open suggestions</button>
        </div>
      )}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          start();
        }}
      >
        <label className="field">
          <span>Scan root (absolute path; empty = home)</span>
          <input
            type="text"
            value={root}
            onChange={(e) => setRoot(e.target.value)}
            placeholder="/home/you"
          />
        </label>
        <label className="field check">
          <input
            type="checkbox"
            checked={hidden}
            onChange={(e) => setHidden(e.target.checked)}
          />
          <span>Include hidden files and directories</span>
        </label>

        <details
          className="scan-paths"
          open={Object.values(defaultTargetPaths ?? {}).some((v) => v.length > 0)}
        >
          <summary>Destination paths (optional)</summary>
          <p className="hint">
            Destination format string per asset kind, relative to home or
            absolute; must contain a placeholder. Empty uses the configured
            default shown as the placeholder.
          </p>
          <div className="field-grid">
            {KINDS.map(([kind, def]) => (
              <label className="field" key={kind}>
                <span>{kind}</span>
                <input
                  type="text"
                  value={paths[kind]}
                  placeholder={pathDefaults[kind] ?? def}
                  onChange={(e) => setPath(kind, e.target.value)}
                />
              </label>
            ))}
          </div>
        </details>

        {form ? (
          <>
            <details className="scan-paths">
              <summary>Scan exclusions (optional)</summary>
              <p className="hint">
                One glob per line, replacing the configured exclusions for this
                scan only. Cleared means no exclusions for this scan.
              </p>
              <textarea
                rows={4}
                value={form.excludePaths}
                onChange={(e) => setForm({ ...form, excludePaths: e.target.value })}
                spellCheck={false}
              />
            </details>

            <details className="scan-paths">
              <summary>Geocoder (%l)</summary>
              <p className="hint">
                Override the geocoder for this scan; empty fields use the
                configured values.
              </p>
              <div className="field-grid">
                <label className="field check">
                  <input
                    type="checkbox"
                    checked={form.geoEnabled}
                    onChange={(e) => patch({ geoEnabled: e.target.checked })}
                  />
                  <span>enabled</span>
                </label>
                <label className="field">
                  <span>service</span>
                  <select
                    value={form.geoService}
                    onChange={(e) => patch({ geoService: e.target.value })}
                  >
                    <option value="">
                      configured ({str(geoDoc.service) || 'nominatim'})
                    </option>
                    {GEOCODER_SERVICES.map((svc) => (
                      <option key={svc} value={svc}>{svc}</option>
                    ))}
                  </select>
                </label>
                <label className="field">
                  <span>endpoint</span>
                  <input
                    type="text"
                    value={form.geoEndpoint}
                    onChange={(e) => patch({ geoEndpoint: e.target.value })}
                  />
                </label>
                <label className="field">
                  <span>api key (sent for this scan only)</span>
                  <input
                    type="password"
                    value={form.geoApiKey}
                    onChange={(e) => patch({ geoApiKey: e.target.value })}
                  />
                </label>
                <label className="field">
                  <span>language (ISO 639-1)</span>
                  <input
                    type="text"
                    value={form.geoLanguage}
                    onChange={(e) => patch({ geoLanguage: e.target.value })}
                  />
                </label>
                <label className="field">
                  <span>user-agent contact</span>
                  <input
                    type="text"
                    value={form.geoContact}
                    onChange={(e) => patch({ geoContact: e.target.value })}
                  />
                </label>
                <label className="field">
                  <span>rate limit (ms)</span>
                  <input
                    type="number"
                    min={0}
                    value={form.geoRate}
                    onChange={(e) => patch({ geoRate: e.target.value })}
                  />
                </label>
              </div>
            </details>

            <details className="scan-paths">
              <summary>Description model (%s)</summary>
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
                    <option value="">configured ({str(descDoc.service) || 'ollama'})</option>
                    {LLM_SERVICES.map((svc) => (
                      <option key={svc} value={svc}>{svc}</option>
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
                  <span>api key (sent for this scan only)</span>
                  <input
                    type="password"
                    value={form.descApiKey}
                    onChange={(e) => patch({ descApiKey: e.target.value })}
                  />
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
            </details>

            <details className="scan-paths">
              <summary>Extraction, limits & cache</summary>
              <p className="hint">
                Document text extraction command per format; arguments are
                split on spaces and {'{path}'} marks the input file. The
                disable checkbox turns extraction off for that format in this
                scan; empty uses the configured (or default) command.
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
                      onChange={(e) => setExtraction(format, e.target.value)}
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
                        setExtractionDisabled(format, e.target.checked)
                      }
                    />
                    <span>disable {format} extraction</span>
                  </label>
                ))}
              </div>
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
            </details>
          </>
        ) : (
          <p className="hint">advanced settings load...</p>
        )}

        <div className="toolbar">
          <button
            type="submit"
            disabled={startDisabled}
            title={startDisabled ? 'a scan or execution is already in progress' : undefined}
          >
            scan
          </button>
        </div>
      </form>
    </main>
  );
}
