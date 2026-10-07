import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../api";

type Json = Record<string, unknown>;

export interface ScanFormValues {
  root: string;
  hidden: boolean;
  targetPaths: Record<string, string>;
}

// The six asset kinds with their built-in default destination format
// strings (DESIGN.md "Destination Path Format Strings > Defaults"); the
// defaults double as input placeholders. Empty input = use the configured
// (or built-in) default for that kind. There are no data, executable, or
// other kinds: those files are unmanaged and never moved or deduplicated.
const KINDS: Array<[string, string]> = [
  ["document", "Documents/%a-%t.%e"],
  ["photo", "Images/%Y-%m-%d-%H-%M-%S-%l-%s.%e"],
  ["video", "Videos/%Y-%m-%d-%H-%M-%S-%l-%s.%e"],
  ["audio", "Audio/%a-%t.%e"],
  ["repository", "Projects/%o/%n"],
  ["archive", "Archives/%Y/%m/%f"],
];

const GEOCODER_SERVICES = ["nominatim", "photon", "google", "mapbox", "custom"];
const LLM_SERVICES = ["ollama", "openrouter", "custom"];

// Document formats the extraction section always offers, with the runtime
// default commands (defaults.toml) as placeholders.
const EXTRACTION_DEFAULTS: Array<[string, string]> = [
  ["pdf", "pdftotext -layout {path} -"],
  ["docx", "pandoc {path} -t plain"],
  ["odt", "pandoc {path} -t plain"],
  ["epub", "pandoc {path} -t plain"],
];

// The configuration-backed sections of this form are live editors of the
// service's persistent configuration: every change is sent to PUT /config
// (debounced, merged, validated server-side) and mirrored everywhere. Only
// the session-scoped scan parameters - root, hidden, target_paths - travel
// with the scan request itself.
//
// Dirty tracking: fields the user has touched are protected from external
// re-seeds until their pending save lands. A per-field touch counter lets a
// completed save clear protection only for values that were actually saved.
interface OverrideState {
  excludePaths: string;
  geoEnabled: boolean;
  geoService: string;
  geoEndpoint: string;
  geoApiKey: string;
  geoLanguage: string;
  geoContact: string;
  geoRate: string;
  geoNeighbourhoodCities: string;
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

const SCALAR_KEYS: Array<keyof OverrideState> = [
  "excludePaths",
  "geoEnabled",
  "geoService",
  "geoEndpoint",
  "geoApiKey",
  "geoLanguage",
  "geoContact",
  "geoRate",
  "geoNeighbourhoodCities",
  "descEnabled",
  "descService",
  "descEndpoint",
  "descModel",
  "descApiKey",
  "descMaxWords",
  "descRate",
  "largeThreshold",
  "providerTimeout",
  "toolTimeout",
  "toolStdoutMax",
  "cacheEnabled",
];

const SAVE_DEBOUNCE_MS = 600;

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

function num(v: unknown): string {
  return typeof v === "number" ? String(v) : "";
}

function cityList(v: unknown): string {
  return Array.isArray(v) ? v.map(String).join(", ") : "";
}

function obj(v: unknown): Json {
  return v !== null && typeof v === "object" && !Array.isArray(v)
    ? (v as Json)
    : {};
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
    new Set([
      ...EXTRACTION_DEFAULTS.map(([k]) => k),
      ...Object.keys(extraction),
    ]),
  );
  const extractionText: Record<string, string> = {};
  const extractionDisabled: Record<string, boolean> = {};
  for (const f of formats) {
    const argv = extraction[f];
    if (Array.isArray(argv) && argv.length === 0) {
      extractionDisabled[f] = true;
      extractionText[f] = "";
    } else {
      extractionDisabled[f] = false;
      extractionText[f] = Array.isArray(argv) ? argv.map(String).join(" ") : "";
    }
  }
  return {
    excludePaths: exclusionsOf(doc).join("\n"),
    geoEnabled: geo.enabled === true,
    geoService: str(geo.service) || "nominatim",
    geoEndpoint: str(geo.endpoint),
    geoApiKey: "",
    geoLanguage: str(geo.language),
    geoContact: str(geo.user_agent_contact),
    geoRate: num(geo.rate_limit_ms),
    geoNeighbourhoodCities: cityList(geo.neighbourhood_cities),
    descEnabled: desc.enabled === true,
    descService: str(desc.service) || "ollama",
    descEndpoint: str(desc.endpoint),
    descModel: str(desc.model),
    descApiKey: "",
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

/// Re-seed untouched fields from a freshly fetched configuration document.
/// Fields the user has touched (dirty) keep their in-form values so an
/// external change never clobbers in-progress edits or pending saves.
function reseedForm(
  cur: OverrideState,
  fresh: OverrideState,
  dirty: Set<string>,
): OverrideState {
  const out: OverrideState = {
    ...cur,
    extraction: { ...cur.extraction },
    extractionDisabled: { ...cur.extractionDisabled },
  };
  const scalars: Partial<OverrideState> = {};
  for (const k of SCALAR_KEYS) {
    if (dirty.has(k)) continue;
    (scalars as Record<string, unknown>)[k] = fresh[k];
  }
  Object.assign(out, scalars);
  for (const [format] of EXTRACTION_DEFAULTS) {
    if (!dirty.has(`extraction.${format}`))
      out.extraction[format] = fresh.extraction[format];
    if (!dirty.has(`extractionDisabled.${format}`)) {
      out.extractionDisabled[format] = fresh.extractionDisabled[format];
    }
  }
  return out;
}

/// Build the partial configuration document for the fields that differ from
/// the baseline document, plus the form-field names that produced it (used
/// to clear dirty protection once the save lands). Cleared optional values
/// are sent as null, which deletes the key and restores the built-in default.
function buildPatch(
  form: OverrideState,
  doc: Json,
): { patch: Json; fields: string[] } {
  const patch: Json = {};
  const fields: string[] = [];

  const patterns = form.excludePaths
    .split("\n")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  if (JSON.stringify(patterns) !== JSON.stringify(exclusionsOf(doc))) {
    patch.scan = { exclude_paths: patterns };
    fields.push("excludePaths");
  }

  const effGeo = obj(doc.geocoder);
  const geo: Json = {};
  if (form.geoEnabled !== (effGeo.enabled === true)) {
    geo.enabled = form.geoEnabled;
    fields.push("geoEnabled");
  }
  if (form.geoService !== str(effGeo.service)) {
    geo.service = form.geoService;
    fields.push("geoService");
  }
  if (form.geoEndpoint.trim() !== str(effGeo.endpoint)) {
    const v = form.geoEndpoint.trim();
    geo.endpoint = v.length > 0 ? v : null;
    fields.push("geoEndpoint");
  }
  if (form.geoApiKey.length > 0) {
    geo.api_key = form.geoApiKey;
    fields.push("geoApiKey");
  }
  if (form.geoLanguage.trim() !== str(effGeo.language)) {
    const v = form.geoLanguage.trim();
    geo.language = v.length > 0 ? v : null;
    fields.push("geoLanguage");
  }
  if (form.geoContact.trim() !== str(effGeo.user_agent_contact)) {
    const v = form.geoContact.trim();
    geo.user_agent_contact = v.length > 0 ? v : null;
    fields.push("geoContact");
  }
  if (form.geoRate.trim() !== num(effGeo.rate_limit_ms)) {
    const n = Number(form.geoRate);
    geo.rate_limit_ms =
      form.geoRate.trim() === "" || Number.isNaN(n) ? null : n;
    fields.push("geoRate");
  }
  if (form.geoNeighbourhoodCities.trim() !== cityList(effGeo.neighbourhood_cities)) {
    geo.neighbourhood_cities = form.geoNeighbourhoodCities
      .split(",")
      .map((c) => c.trim())
      .filter((c) => c.length > 0);
    fields.push("geoNeighbourhoodCities");
  }
  if (Object.keys(geo).length > 0) patch.geocoder = geo;

  const effDesc = obj(doc.description);
  const desc: Json = {};
  if (form.descEnabled !== (effDesc.enabled === true)) {
    desc.enabled = form.descEnabled;
    fields.push("descEnabled");
  }
  if (form.descService !== str(effDesc.service)) {
    desc.service = form.descService;
    fields.push("descService");
  }
  if (form.descEndpoint.trim() !== str(effDesc.endpoint)) {
    const v = form.descEndpoint.trim();
    desc.endpoint = v.length > 0 ? v : null;
    fields.push("descEndpoint");
  }
  if (form.descModel.trim() !== str(effDesc.model)) {
    const v = form.descModel.trim();
    desc.model = v.length > 0 ? v : null;
    fields.push("descModel");
  }
  if (form.descApiKey.length > 0) {
    desc.api_key = form.descApiKey;
    fields.push("descApiKey");
  }
  if (form.descMaxWords.trim() !== num(effDesc.max_words)) {
    const n = Number(form.descMaxWords);
    desc.max_words =
      form.descMaxWords.trim() === "" || Number.isNaN(n) ? null : n;
    fields.push("descMaxWords");
  }
  if (form.descRate.trim() !== num(effDesc.rate_limit_ms)) {
    const n = Number(form.descRate);
    desc.rate_limit_ms =
      form.descRate.trim() === "" || Number.isNaN(n) ? null : n;
    fields.push("descRate");
  }
  if (Object.keys(desc).length > 0) patch.description = desc;

  const effDup = obj(doc.duplication);
  if (form.largeThreshold.trim() !== num(effDup.large_file_threshold)) {
    const n = Number(form.largeThreshold);
    patch.duplication = {
      large_file_threshold:
        form.largeThreshold.trim() === "" || Number.isNaN(n) ? null : n,
    };
    fields.push("largeThreshold");
  }

  const effLim = obj(doc.limits);
  const lim: Json = {};
  const limit = (loaded: unknown, value: string): unknown => {
    if (value.trim() === num(loaded)) return undefined;
    const n = Number(value);
    return value.trim() === "" || Number.isNaN(n) ? null : n;
  };
  const p = limit(effLim.provider_timeout_seconds, form.providerTimeout);
  if (p !== undefined) {
    lim.provider_timeout_seconds = p;
    fields.push("providerTimeout");
  }
  const t = limit(effLim.tool_timeout_seconds, form.toolTimeout);
  if (t !== undefined) {
    lim.tool_timeout_seconds = t;
    fields.push("toolTimeout");
  }
  const s = limit(effLim.tool_stdout_max_bytes, form.toolStdoutMax);
  if (s !== undefined) {
    lim.tool_stdout_max_bytes = s;
    fields.push("toolStdoutMax");
  }
  if (Object.keys(lim).length > 0) patch.limits = lim;

  if (form.cacheEnabled !== (obj(doc.cache).enabled !== false)) {
    patch.cache = { enabled: form.cacheEnabled };
    fields.push("cacheEnabled");
  }

  const effEx = obj(doc.document_extraction) as Record<string, unknown>;
  const ex: Record<string, unknown> = {};
  for (const [format] of EXTRACTION_DEFAULTS) {
    const initial = effEx[format];
    const initialArr = Array.isArray(initial) ? initial.map(String) : null;
    if (form.extractionDisabled[format]) {
      if (initialArr === null || initialArr.length > 0) {
        ex[format] = [];
        fields.push(`extractionDisabled.${format}`);
      }
      continue;
    }
    const argv = form.extraction[format]
      .trim()
      .split(/\s+/)
      .filter((a) => a.length > 0);
    if (argv.length === 0) {
      // Cleared while the config carries a custom or disabled command:
      // reset to the built-in default tool.
      if (initialArr !== null && initialArr.length > 0) {
        ex[format] = null;
        fields.push(`extraction.${format}`);
      }
      continue;
    }
    if (initialArr === null || argv.join(" ") !== initialArr.join(" ")) {
      ex[format] = argv;
      fields.push(`extraction.${format}`);
    }
  }
  if (Object.keys(ex).length > 0) patch.document_extraction = ex;

  return { patch, fields };
}

function mergePatch(a: Json, b: Json): Json {
  const out: Json = { ...a };
  for (const [k, v] of Object.entries(b)) {
    const cur = out[k];
    if (
      v !== null &&
      typeof v === "object" &&
      !Array.isArray(v) &&
      cur !== null &&
      cur !== undefined &&
      typeof cur === "object" &&
      !Array.isArray(cur)
    ) {
      out[k] = { ...(cur as Json), ...(v as Json) };
    } else {
      out[k] = v;
    }
  }
  return out;
}

interface ScanFormProps {
  defaultRoot: string;
  defaultHidden?: boolean;
  defaultTargetPaths?: Record<string, string>;
  /** Hash of the service's live configuration; a change re-seeds untouched fields. */
  configHash: string | null;
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
    configHash,
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
      init[kind] = defaultTargetPaths?.[kind] ?? "";
    }
    return init;
  });
  const [doc, setDoc] = useState<Json | null>(null);
  const [form, setForm] = useState<OverrideState | null>(null);
  const [saveNote, setSaveNote] = useState<string | null>(null);

  const dirtyRef = useRef<Set<string>>(new Set());
  const touchRef = useRef<Map<string, number>>(new Map());
  const counterRef = useRef(0);
  const pendingRef = useRef<{ patch: Json; fields: Set<string> } | null>(null);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const savingRef = useRef(false);
  const formRef = useRef<OverrideState | null>(null);
  const docRef = useRef<Json | null>(null);
  formRef.current = form;
  docRef.current = doc;

  const markDirty = (keys: string[]) => {
    for (const k of keys) {
      dirtyRef.current.add(k);
      touchRef.current.set(k, ++counterRef.current);
    }
  };

  const flush = useCallback(async (): Promise<boolean> => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    const pending = pendingRef.current;
    if (pending === null) return true;
    pendingRef.current = null;
    // Snapshot the touch counters of the fields being saved: a field edited
    // again while the request is in flight keeps its dirty protection.
    const snapshot = new Map<string, number>();
    for (const k of pending.fields) {
      const c = touchRef.current.get(k);
      if (c !== undefined) snapshot.set(k, c);
    }
    savingRef.current = true;
    try {
      const updated = await api.configPut(pending.patch);
      setDoc(updated);
      setSaveNote(null);
      for (const [k, c] of snapshot) {
        if (touchRef.current.get(k) === c) dirtyRef.current.delete(k);
      }
      // API keys are write-only: clear the input once stored.
      if (pending.fields.has("geoApiKey"))
        setForm((f) => (f ? { ...f, geoApiKey: "" } : f));
      if (pending.fields.has("descApiKey"))
        setForm((f) => (f ? { ...f, descApiKey: "" } : f));
      return true;
    } catch (e: unknown) {
      // Re-queue so the next edit (or scan start) retries the save.
      pendingRef.current = pending;
      setSaveNote(
        `saving settings failed: ${e instanceof Error ? e.message : String(e)}`,
      );
      return false;
    } finally {
      savingRef.current = false;
    }
  }, []);

  const scheduleSave = useCallback(
    (p: Partial<OverrideState>) => {
      const { patch, fields: changedFields } = buildPatch(
        { ...formRef.current!, ...p },
        docRef.current!,
      );
      markDirty(changedFields);
      // Group the fields buildPatch changed by their configuration section
      // and keep only those values, so a save never carries untouched state.
      const sectionFields = new Set(changedFields);
      const relevant: Json = {};
      const bySection: Record<string, string[]> = {};
      const sectionFor: Record<string, string> = {
        excludePaths: "scan",
        geoEnabled: "geocoder",
        geoService: "geocoder",
        geoEndpoint: "geocoder",
        geoApiKey: "geocoder",
        geoLanguage: "geocoder",
        geoContact: "geocoder",
        geoRate: "geocoder",
        geoNeighbourhoodCities: "geocoder",
        descEnabled: "description",
        descService: "description",
        descEndpoint: "description",
        descModel: "description",
        descApiKey: "description",
        descMaxWords: "description",
        descRate: "description",
        largeThreshold: "duplication",
        providerTimeout: "limits",
        toolTimeout: "limits",
        toolStdoutMax: "limits",
        cacheEnabled: "cache",
      };
      for (const f of sectionFields) {
        const section = f.includes(".")
          ? "document_extraction"
          : (sectionFor[f] ?? f);
        bySection[section] = bySection[section] ?? [];
        bySection[section].push(f);
      }
      for (const [section, keys] of Object.entries(bySection)) {
        const value = patch[section];
        if (value === undefined || value === null || typeof value !== "object")
          continue;
        const entry: Json = {};
        const sectionObj = value as Json;
        if (section === "scan") {
          if ("exclude_paths" in sectionObj)
            entry.exclude_paths = sectionObj.exclude_paths;
        } else if (section === "duplication") {
          if ("large_file_threshold" in sectionObj) {
            entry.large_file_threshold = sectionObj.large_file_threshold;
          }
        } else if (section === "cache") {
          if ("enabled" in sectionObj) entry.enabled = sectionObj.enabled;
        } else if (section === "document_extraction") {
          for (const f of keys)
            entry[f.split(".")[1]] = sectionObj[f.split(".")[1]];
        } else {
          const keyByField: Record<string, string> = {
            geoEnabled: "enabled",
            geoService: "service",
            geoEndpoint: "endpoint",
            geoApiKey: "api_key",
            geoLanguage: "language",
            geoContact: "user_agent_contact",
            geoRate: "rate_limit_ms",
            geoNeighbourhoodCities: "neighbourhood_cities",
            descEnabled: "enabled",
            descService: "service",
            descEndpoint: "endpoint",
            descModel: "model",
            descApiKey: "api_key",
            descMaxWords: "max_words",
            descRate: "rate_limit_ms",
            providerTimeout: "provider_timeout_seconds",
            toolTimeout: "tool_timeout_seconds",
            toolStdoutMax: "tool_stdout_max_bytes",
          };
          for (const f of keys) {
            const configKey = keyByField[f];
            if (configKey && configKey in sectionObj)
              entry[configKey] = sectionObj[configKey];
          }
        }
        if (Object.keys(entry).length > 0) relevant[section] = entry;
      }
      if (Object.keys(relevant).length === 0) return;
      pendingRef.current = pendingRef.current
        ? {
            patch: mergePatch(pendingRef.current.patch, relevant),
            fields: new Set([...pendingRef.current.fields, ...sectionFields]),
          }
        : { patch: relevant, fields: sectionFields };
      if (timerRef.current === null) {
        timerRef.current = setTimeout(() => {
          timerRef.current = null;
          void flush();
        }, SAVE_DEBOUNCE_MS);
      }
    },
    [flush],
  );

  // Initial load, then mirror the service: when the live config hash moves
  // (config.updated event or the 2s session poll), re-seed untouched fields
  // from the fresh document. Skipped while a save is pending or in flight;
  // the save's own config.updated event re-triggers this afterwards.
  const hashRef = useRef<string | null>(null);
  const refreshDoc = useCallback(async () => {
    if (pendingRef.current !== null || savingRef.current) return;
    try {
      const c = await api.config();
      setDoc(c);
      setForm((f) =>
        f ? reseedForm(f, formFromDoc(c), dirtyRef.current) : formFromDoc(c),
      );
    } catch {
      // The next hash change or poll retries.
    }
  }, []);
  useEffect(() => {
    void refreshDoc();
  }, [refreshDoc]);
  useEffect(() => {
    const previous = hashRef.current;
    hashRef.current = configHash;
    if (configHash === null || previous === null || previous === configHash)
      return;
    void refreshDoc();
  }, [configHash, refreshDoc]);

  // Best-effort flush of unsaved edits when the form unmounts.
  useEffect(
    () => () => {
      if (pendingRef.current !== null) void flush();
    },
    [flush],
  );

  const setPath = (kind: string, value: string) =>
    setPaths((prev) => ({ ...prev, [kind]: value }));

  const patch = (p: Partial<OverrideState>) => {
    setForm((f) => (f ? { ...f, ...p } : f));
    scheduleSave(p);
  };

  const setExtraction = (format: string, value: string) => {
    setForm((f) =>
      f ? { ...f, extraction: { ...f.extraction, [format]: value } } : f,
    );
    scheduleSave({});
  };

  const setExtractionDisabled = (format: string, value: boolean) => {
    setForm((f) =>
      f
        ? {
            ...f,
            extractionDisabled: { ...f.extractionDisabled, [format]: value },
          }
        : f,
    );
    scheduleSave({});
  };

  const pathDefaults = obj(doc?.target_path_defaults) as Record<string, string>;
  const configWarnings = Object.entries(obj(doc?.warnings));

  const start = async () => {
    if (!(await flush())) {
      setSaveNote("settings could not be saved; scan not started");
      return;
    }
    const targetPaths: Record<string, string> = {};
    for (const [kind] of KINDS) {
      const t = (paths[kind] ?? "").trim();
      if (t.length > 0) targetPaths[kind] = t;
    }
    onStart({
      root: root.trim(),
      hidden,
      targetPaths: paths,
    });
  };

  return (
    <main className="scan-form">
      <h1>homelinter</h1>
      <p className="subtitle">
        Duplicate and home cleanup suggestions for your files
      </p>
      {hasSuggestions && (
        <div className="resume">
          <span>An existing scan session has suggestions ready.</span>
          <button type="button" onClick={onResume}>
            open suggestions
          </button>
        </div>
      )}
      <p className="hint">
        The settings below are the service's persistent configuration: changes
        are saved to the service as you edit and apply to the next scan. Only
        the scan root, hidden files, and destination paths are per-scan choices
        and are never saved.
      </p>
      {saveNote !== null && <p className="error">{saveNote}</p>}
      {configWarnings.length > 0 && (
        <div className="config-warnings" role="status">
          {configWarnings.map(([field, reason]) => (
            <p key={field} className="note warn">
              <strong>{field}:</strong> {String(reason)}
            </p>
          ))}
        </div>
      )}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void start();
        }}
      >
        <label className="field">
          <span>Scan root (absolute path; empty = home; per scan)</span>
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
          <span>Include hidden files and directories (per scan)</span>
        </label>

        <details
          className="scan-paths"
          open={Object.values(defaultTargetPaths ?? {}).some(
            (v) => v.length > 0,
          )}
        >
          <summary>Destination paths (optional; per scan)</summary>
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
              <summary>Scan exclusions</summary>
              <p className="hint">
                One glob per line; saved to the configuration as you edit. An
                empty list means no exclusions.
              </p>
              <textarea
                rows={4}
                value={form.excludePaths}
                onChange={(e) => patch({ excludePaths: e.target.value })}
                spellCheck={false}
              />
            </details>

            <details className="scan-paths">
              <summary>Geocoder (%l)</summary>
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
                    {GEOCODER_SERVICES.map((svc) => (
                      <option key={svc} value={svc}>
                        {svc}
                      </option>
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
                  <span>api key (write-only; saved when set)</span>
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
                  <span>neighbourhood cities</span>
                  <input
                    type="text"
                    placeholder="e.g. Toronto, Shibuya"
                    value={form.geoNeighbourhoodCities}
                    onChange={(e) =>
                      patch({ geoNeighbourhoodCities: e.target.value })
                    }
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
                    {LLM_SERVICES.map((svc) => (
                      <option key={svc} value={svc}>
                        {svc}
                      </option>
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
                  <span>api key (write-only; saved when set)</span>
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
                Document text extraction command per format; arguments are split
                on spaces and {"{path}"} marks the input file. The disable
                checkbox turns extraction off for that format; empty uses the
                built-in default command. All values are saved to the
                configuration as you edit.
              </p>
              <div className="field-grid">
                {EXTRACTION_DEFAULTS.map(([format, def]) => (
                  <label className="field" key={format}>
                    <span>{format}</span>
                    <input
                      type="text"
                      value={form.extraction[format] ?? ""}
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
          <p className="hint">settings load...</p>
        )}

        <div className="toolbar">
          <button
            type="submit"
            disabled={startDisabled}
            title={
              startDisabled
                ? "a scan or execution is already in progress"
                : undefined
            }
          >
            scan
          </button>
        </div>
      </form>
    </main>
  );
}
