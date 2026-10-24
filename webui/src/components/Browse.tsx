import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
  ReactNode,
} from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import {
  ALL_FILTER,
  Filter,
  FilterOption,
  MANUAL_OPTIONS,
  STATUS_OPTIONS,
  TYPE_OPTIONS,
  isDefaultFilter,
} from '../lib/filter';
import {
  manualOverrideLabel,
  metadataProgressLabel,
  rowText,
  rowTone,
  suggestionCountLabel,
} from '../lib/format';
import { previewSourcePath } from '../lib/preview';
import { useEscapeClose } from '../lib/modal';
import type { MetadataProgress, Suggestion } from '../types';
import { CloseIcon } from './CloseIcon';
import { CheckIcon, CopyIcon } from './CopyIcon';
import { ExpandIcon } from './ExpandIcon';
import { RefreshIcon } from './RefreshIcon';


type TabId = 'preview' | 'location' | 'metadata';

interface BrowseProps {
  suggestions: Suggestion[];
  /** Complete set of asset kinds for the batch (from the report summary). */
  kinds: string[];
  filter: Filter;
  /** Every suggestion in the batch, before filtering. */
  totalSuggestions: number;
  /** Suggestions matching the active filter, as counted by the service. */
  filteredSuggestions: number;
  /** Selected suggestions across the whole batch. */
  selectedCount: number;
  /** Suggestions with at least one saved manual metadata override. */
  manualCount: number;
  metadataProgress?: MetadataProgress | null;
  /** Provider cache lookups for the current batch (0 until a provider
   *  request is made; the pair is hidden while both are zero). */
  cacheHits: number;
  cacheMisses: number;
  actionsDisabled?: boolean;
  root?: string | null;
  onFilterChange: (filter: Filter) => void;
  onLoadMore: () => void;
  onToggle: (id: string, select: boolean) => void;
  onSelectFiltered: (filter: Filter) => void;
  onClearFiltered: (filter: Filter) => void;
  onRun: () => void;
  onExport: () => void;
  onRefreshMetadata: (id: string, clearOverrides?: boolean) => void;
  onSetMetadata: (
    id: string,
    edit: { location: string; subject: string; author: string },
  ) => void;
  onRebuild: (id: string) => void;
}

// The service falls back to a bare "lat,lng" string when reverse geocoding
// fails, so a bare decimal pair is not a place name.
const COORDS_ONLY = /^\s*-?\d+(\.\d+)?\s*,\s*-?\d+(\.\d+)?\s*$/;

type PreviewKind = 'image' | 'video' | 'audio' | 'pdf' | 'text';

function previewKind(path: string): PreviewKind | null {
  const ext = path.split('.').pop()?.toLowerCase();
  if (['jpg', 'jpeg', 'png', 'gif', 'webp', 'avif', 'svg'].includes(ext ?? '')) return 'image';
  if (['mp4', 'm4v', 'webm', 'mov'].includes(ext ?? '')) return 'video';
  if (['mp3', 'wav', 'ogg', 'oga', 'm4a'].includes(ext ?? '')) return 'audio';
  if (ext === 'pdf') return 'pdf';
  if (['txt', 'md', 'csv', 'log'].includes(ext ?? '')) return 'text';
  return null;
}

const META_LABELS: Record<string, string> = {
  extracted_date: 'Date taken',
  date_source: 'Date source',
  date_offset: 'UTC offset',
  size: 'Size (bytes)',
  bitrate_kbps: 'Bitrate (kbps)',
  duration_secs: 'Duration (s)',
  page_count: 'Pages',
  camera: 'Camera',
  title: 'Title',
  author: 'Author',
  latitude: 'Latitude',
  longitude: 'Longitude',
  location: 'Location name',
};

function metaKeyLabel(key: string): string {
  return META_LABELS[key] ?? key.replace(/_/g, ' ');
}

function metaValueLabel(value: unknown): string {
  if (typeof value === 'number') return value.toLocaleString('en-US');
  if (typeof value === 'string') return value;
  return JSON.stringify(value);
}

/** Table rows for the metadata tab; duplicate_of is shown in the preview. */
function metaRows(s: Suggestion): [string, unknown][] {
  return Object.entries(s.metadata ?? {}).filter(([k]) => k !== 'duplicate_of');
}

/** Discard the cached provider metadata for one move suggestion and ask for
 *  it again. Delete suggestions carry no provider metadata, so it is not
 *  offered for them. */
function FetchMetadataButton(props: {
  suggestion: Suggestion;
  onRefresh: (id: string, clearOverrides?: boolean) => void;
}) {
  const { suggestion, onRefresh } = props;
  const [confirming, setConfirming] = useState(false);
  useEscapeClose(confirming, () => setConfirming(false));
  if (suggestion.type !== 'move') return null;
  const pending = suggestion.metadata_pending;
  const manual = manualOverrideLabel(suggestion.manual_overrides);
  const confirm = () => {
    setConfirming(false);
    onRefresh(suggestion.id, true);
  };
  return (
    <>
      <button
        type="button"
        className="meta-fetch-btn"
        disabled={pending}
        title={
          manual
            ? 'discard the cached provider results and ask again; ' +
              `this clears the manual overrides for ${manual}`
            : 'discard the cached provider results and ask again'
        }
        onClick={() => {
          if (manual) setConfirming(true);
          else onRefresh(suggestion.id);
        }}
      >
        {pending ? 'fetching...' : 'fetch metadata'}
      </button>
      {confirming && (
        <div
          className="modal-backdrop"
          role="presentation"
          onClick={() => setConfirming(false)}
        >
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            onClick={(e) => e.stopPropagation()}
          >
            <h2>Clear manual metadata overrides?</h2>
            <p>
              This suggestion's {manual}{' '}
              {suggestion.manual_overrides?.length === 1 ? 'is' : 'are'} set by
              manual overrides. Fetching metadata will clear those overrides and
              use provider or embedded metadata instead. This cannot be undone.
            </p>
            <div className="modal-actions">
              <button
                type="button"
                autoFocus
                onClick={() => setConfirming(false)}
              >
                cancel
              </button>
              <button type="button" className="danger" onClick={confirm}>
                clear and fetch
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}

/** Rebuild this move's destination from the current effective metadata without
 *  a scan or a provider call. Rendered as an icon button beside the `to` path's
 *  copy action. */
function RebuildButton(props: {
  suggestion: Suggestion;
  onRebuild: (id: string) => void;
}) {
  const { suggestion, onRebuild } = props;
  if (suggestion.type !== 'move') return null;
  return (
    <button
      type="button"
      className="path-action-btn"
      title="rebuild the destination from the current metadata"
      aria-label="rebuild the destination from the current metadata"
      onClick={() => onRebuild(suggestion.id)}
    >
      <RefreshIcon />
    </button>
  );
}

/** Copy `value` to the clipboard, using the async Clipboard API when it is
 *  available and falling back to a hidden textarea + `execCommand` otherwise
 *  (for example a non-secure context). Resolves to whether the copy worked. */
async function copyText(value: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(value);
      return true;
    }
  } catch {
    // Fall through to the legacy path.
  }
  try {
    const el = document.createElement('textarea');
    el.value = value;
    el.style.position = 'fixed';
    el.style.opacity = '0';
    document.body.appendChild(el);
    el.select();
    const ok = document.execCommand('copy');
    document.body.removeChild(el);
    return ok;
  } catch {
    return false;
  }
}

/** One source/destination path in the preview. Both the path text and a
 *  trailing button copy the full path to the clipboard; the button confirms
 *  the copy for a moment. */
function PathRow(props: { label: string; value: string; action?: ReactNode }) {
  const { label, value, action } = props;
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );
  const copy = async () => {
    if (await copyText(value)) {
      setCopied(true);
      if (timer.current !== null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1500);
    }
  };
  return (
    <p className="detail-path">
      <span className="detail-label">{label} </span>
      <span
        className="path-copy"
        role="button"
        tabIndex={0}
        title="copy path to clipboard"
        onClick={() => void copy()}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            void copy();
          }
        }}
      >
        {value}
      </span>
      <button
        type="button"
        className="path-copy-btn"
        title={copied ? 'copied' : 'copy path to clipboard'}
        aria-label={copied ? 'copied' : 'copy path to clipboard'}
        onClick={() => void copy()}
      >
        {copied ? <CheckIcon /> : <CopyIcon />}
      </button>
      {action}
    </p>
  );
}

/** The preview media for a suggestion, shared by the inline pane and the
 *  full-screen modal. `enlarged` drops the pane-sized caps so the media fills
 *  the modal. */
function PreviewContent(props: {
  suggestion: Suggestion;
  kind: PreviewKind;
  enlarged?: boolean;
}) {
  const { suggestion, kind, enlarged } = props;
  const src = `/api/v1/suggestions/${encodeURIComponent(suggestion.id)}/preview`;
  const cls = (base: string) => (enlarged ? `${base} preview-enlarged` : base);
  switch (kind) {
    case 'image':
      return <img className={cls('file-preview-image')} src={src} alt="file preview" />;
    case 'video':
      return (
        <video className={cls('file-preview-media')} controls preload="metadata" src={src} />
      );
    case 'audio':
      return (
        <audio className={cls('file-preview-audio')} controls preload="metadata" src={src} />
      );
    case 'pdf':
      return <iframe className={cls('file-preview-pdf')} src={src} title="PDF preview" />;
    case 'text':
      return <iframe className={cls('file-preview-text')} src={src} title="text file preview" />;
  }
}

/** Manual metadata editor: the three values that shape a move's destination
 *  (`%l`, `%s`/`%t`, `%a`). Saving replaces all three overrides for this
 *  content, and the service persists them keyed by content identity. The
 *  fields start from the effective metadata (a manual edit, a provider
 *  value, or embedded metadata) so a fetch is a starting point, not a
 *  reset. */
function MetadataEditor(props: {
  suggestion: Suggestion;
  onSave: (
    id: string,
    edit: { location: string; subject: string; author: string },
  ) => void;
  /** Extra action rendered beside the save button (the fetch button). */
  actions?: ReactNode;
}) {
  const { suggestion, onSave, actions } = props;
  const m = suggestion.metadata ?? {};
  const overridden = new Set(suggestion.manual_overrides ?? []);
  const effective = {
    location: typeof m.location === 'string' ? m.location : '',
    subject: typeof m.title === 'string' ? m.title : '',
    author: typeof m.author === 'string' ? m.author : '',
  };
  const [location, setLocation] = useState(effective.location);
  const [subject, setSubject] = useState(effective.subject);
  const [author, setAuthor] = useState(effective.author);
  // Reset the fields whenever the selected suggestion or its effective
  // metadata changes (opening another row, a provider resolution, or a
  // saved edit coming back from the service).
  useEffect(() => {
    setLocation(effective.location);
    setSubject(effective.subject);
    setAuthor(effective.author);
  }, [suggestion.id, effective.location, effective.subject, effective.author]);

  return (
    <form
      className="meta-edit"
      onSubmit={(e) => {
        e.preventDefault();
        onSave(suggestion.id, { location, subject, author });
      }}
    >
      <label className="field">
        <span className="field-label">
          Location (%l){overridden.has('location') ? ' - manual' : ''}
        </span>
        <input
          value={location}
          onChange={(e) => setLocation(e.target.value)}
          placeholder="leave blank to use geocoder or embedded GPS"
        />
      </label>
      <label className="field">
        <span className="field-label">
          Subject (%s, %t){overridden.has('subject') ? ' - manual' : ''}
        </span>
        <input
          value={subject}
          onChange={(e) => setSubject(e.target.value)}
          placeholder="leave blank to use embedded title or a description"
        />
      </label>
      <label className="field">
        <span className="field-label">
          Author (%a){overridden.has('author') ? ' - manual' : ''}
        </span>
        <input
          value={author}
          onChange={(e) => setAuthor(e.target.value)}
          placeholder="leave blank to use embedded or inferred author"
        />
      </label>
      <div className="meta-actions">
        <button type="submit" className="meta-save-btn">
          save metadata
        </button>
        {actions}
      </div>
    </form>
  );
}

function DetailPane(props: {
  suggestion: Suggestion;
  onClose: () => void;
  onRefreshMetadata: (id: string, clearOverrides?: boolean) => void;
  onSetMetadata: (
    id: string,
    edit: { location: string; subject: string; author: string },
  ) => void;
  onRebuild: (id: string) => void;
}) {
  const { suggestion, onClose, onRefreshMetadata, onSetMetadata, onRebuild } = props;
  const [tab, setTab] = useState<TabId>('preview');
  // The enlarged preview and the enlarged map share one modal state, so
  // only one can be open and Escape dismisses whichever it is through the
  // shared modal stack (lib/modal), like every other dialog.
  const [expanded, setExpanded] = useState<'preview' | 'location' | null>(null);
  useEscapeClose(expanded !== null, () => setExpanded(null));
  // An executed suggestion's target no longer exists, so the preview element
  // is chosen from the file's post-execution path (see previewSourcePath).
  const kind = previewKind(previewSourcePath(suggestion));

  const m = suggestion.metadata ?? {};
  const lat = typeof m.latitude === 'number' ? m.latitude : null;
  const lon = typeof m.longitude === 'number' ? m.longitude : null;
  const placeName =
    typeof m.location === 'string' &&
    m.location.length > 0 &&
    !COORDS_ONLY.test(m.location)
      ? m.location
      : null;
  const rows = useMemo(() => metaRows(suggestion), [suggestion]);

  const tabs: TabId[] = ['preview'];
  if (lat !== null && lon !== null) tabs.push('location');
  // A move always gets the metadata tab, even before any provider value
  // arrives, so manual editing is always reachable.
  if (suggestion.type === 'move' || rows.length > 0) tabs.push('metadata');
  const active: TabId = tabs.includes(tab) ? tab : 'preview';

  const mapSrc =
    lat !== null && lon !== null
      ? `https://www.openstreetmap.org/export/embed.html?bbox=${(lon - 0.01).toFixed(4)}%2C${(lat - 0.01).toFixed(4)}%2C${(lon + 0.01).toFixed(4)}%2C${(lat + 0.01).toFixed(4)}&layer=mapnik&marker=${lat}%2C${lon}`
      : null;

  return (
    <aside className="detail-pane">
      <div className="detail-tabs">
        {tabs.map((t) => (
          <button
            key={t}
            type="button"
            className={`tab-btn${active === t ? ' active' : ''}`}
            onClick={() => setTab(t)}
          >
            {t}
          </button>
        ))}
        <span className="grow" />
        {active === 'preview' && kind !== null && (
          <button
            type="button"
            className="close-btn"
            title="enlarge preview"
            aria-label="enlarge preview"
            onClick={() => setExpanded('preview')}
          >
            <ExpandIcon />
          </button>
        )}
        {active === 'location' && mapSrc !== null && (
          <button
            type="button"
            className="close-btn"
            title="enlarge map"
            aria-label="enlarge map"
            onClick={() => setExpanded('location')}
          >
            <ExpandIcon />
          </button>
        )}
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

      {active === 'preview' && (
        <div className="detail-body">
          {kind !== null && <PreviewContent suggestion={suggestion} kind={kind} />}
          {suggestion.type === 'move' ? (
            <>
              <p className="detail-action">move</p>
              <PathRow label="from:" value={suggestion.target} />
              <PathRow
                label="to:"
                value={suggestion.new_path ?? '?'}
                action={
                  <RebuildButton suggestion={suggestion} onRebuild={onRebuild} />
                }
              />
            </>
          ) : (
            <>
              <p className="detail-action">delete</p>
              <PathRow label="target:" value={suggestion.target} />
              {typeof m.duplicate_of === 'string' && m.duplicate_of.length > 0 && (
                <PathRow label="original:" value={m.duplicate_of} />
              )}
            </>
          )}
          {suggestion.reason && (
            <p className="detail-reason">{suggestion.reason}</p>
          )}
          {suggestion.rejection_reason && (
            <p className="detail-rejected">
              <span className="detail-label">rejected: </span>
              {suggestion.rejection_reason}
            </p>
          )}
        </div>
      )}

      {active === 'location' && lat !== null && lon !== null && (
        <div className="detail-body">
          <p className="detail-coords">
            {lat.toFixed(6)}, {lon.toFixed(6)}
          </p>
          {placeName && <p className="detail-place">{placeName}</p>}
          {mapSrc && (
            <iframe className="map-frame" title="location map" src={mapSrc} />
          )}
        </div>
      )}

      {active === 'metadata' && (
        <div className="detail-body">
          <table className="detail-table">
            <tbody>
              {rows.map(([key, value]) => (
                <tr key={key}>
                  <th scope="row">{metaKeyLabel(key)}</th>
                  <td>{metaValueLabel(value)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <MetadataEditor
            suggestion={suggestion}
            onSave={onSetMetadata}
            actions={
              <FetchMetadataButton
                suggestion={suggestion}
                onRefresh={onRefreshMetadata}
              />
            }
          />
        </div>
      )}

      {expanded !== null && (
        <div
          className="modal-backdrop preview-backdrop"
          role="presentation"
          onClick={() => setExpanded(null)}
        >
          <div
            className="preview-modal"
            role="dialog"
            aria-modal="true"
            aria-label={expanded === 'location' ? 'location map' : 'file preview'}
            onClick={(e) => e.stopPropagation()}
          >
            <button
              type="button"
              className="close-btn preview-modal-close"
              autoFocus
              aria-label="close"
              title="close"
              onClick={() => setExpanded(null)}
            >
              <CloseIcon />
            </button>
            {expanded === 'location' ? (
              <iframe
                className="map-frame map-frame-enlarged"
                title="location map"
                src={mapSrc ?? undefined}
              />
            ) : (
              kind !== null && (
                <PreviewContent suggestion={suggestion} kind={kind} enlarged />
              )
            )}
          </div>
        </div>
      )}
    </aside>
  );
}

// Virtualization needs a stable row height. Rows are a single ellipsized
// line (see `.list-row`/`.row-hit` in styles.css), so a fixed size is exact
// and avoids per-row measurement.
const ROW_HEIGHT = 29;

interface SuggestionRowProps {
  suggestion: Suggestion;
  root: string | null;
  onToggle: (id: string, select: boolean) => void;
  onOpen: (id: string) => void;
  onToggleManualFilter: () => void;
}

/** One list row. Memoized so a checkbox toggle or a single provider-metadata
 *  update only reconciles the rows whose props actually changed. */
const SuggestionRow = memo(function SuggestionRow({
  suggestion,
  root,
  onToggle,
  onOpen,
  onToggleManualFilter,
}: SuggestionRowProps) {
  const overrides = suggestion.manual_overrides ?? [];
  // Finished rows carry their outcome as the row's colour rather than a
  // status suffix: green and struck for done, red for rejected/failed,
  // orange for partial, dim for skipped.
  const tone = rowTone(suggestion.status);
  return (
    <>
      <input
        type="checkbox"
        className="row-check"
        checked={suggestion.selected && suggestion.status !== 'executed'}
        disabled={suggestion.status === 'executed'}
        onChange={() => onToggle(suggestion.id, !suggestion.selected)}
        aria-label="select"
      />
      <button
        type="button"
        className="row-hit"
        onClick={() => onOpen(suggestion.id)}
      >
        <span
          className={
            tone === null
              ? 'row-text'
              : `row-text row-status-${tone}`
          }
        >
          {suggestion.metadata_pending && (
            <span className="spinner" aria-hidden="true" />
          )}
          {rowText(suggestion, root)}
        </span>
        {overrides.length > 0 && (
          <span
            className="row-badge"
            title={`manual overrides: ${manualOverrideLabel(overrides)}; click to filter`}
            onClick={(e) => {
              e.stopPropagation();
              onToggleManualFilter();
            }}
          >
            manual
          </span>
        )}
      </button>
    </>
  );
});

export function Browse(props: BrowseProps) {
  const { suggestions, kinds, filter, totalSuggestions, filteredSuggestions, selectedCount, manualCount, metadataProgress, cacheHits, cacheMisses, actionsDisabled, root, onFilterChange, onLoadMore, onToggle, onSelectFiltered, onClearFiltered, onRun, onExport, onRefreshMetadata, onSetMetadata, onRebuild } = props;
  const [openId, setOpenId] = useState<string | null>(null);
  const scrollRef = useRef<HTMLElement | null>(null);
  const panesRef = useRef<HTMLDivElement | null>(null);
  // The detail pane width is driven by a draggable divider and clamped so
  // neither pane can be dragged below a usable minimum.
  const [detailWidth, setDetailWidth] = useState(360);
  const dragRef = useRef<{ startX: number; startWidth: number } | null>(null);

  // Execute and export need both an actionable session state and a non-empty
  // selection; the disabled reason says which of the two is missing.
  const nothingSelected = selectedCount === 0;
  const executeDisabled = actionsDisabled || nothingSelected;
  const exportDisabled = actionsDisabled || nothingSelected;
  const stateReason = (action: string) =>
    `${action} is only available from a ready or done session`;
  const executeTitle = actionsDisabled
    ? stateReason('execute')
    : nothingSelected
      ? 'select at least one suggestion first'
      : undefined;
  const exportTitle = actionsDisabled
    ? stateReason('export')
    : nothingSelected
      ? 'select at least one suggestion first'
      : undefined;

  const kindOptions = useMemo<FilterOption<'all' | string>[]>(() => {
    // Keep an active kind selectable even if the batch summary omits it.
    const active = filter.kind !== 'all' && !kinds.includes(filter.kind) ? [filter.kind] : [];
    return [
      { value: 'all', label: 'any type' },
      ...[...active, ...kinds].map((k) => ({ value: k, label: k })),
    ];
  }, [kinds, filter.kind]);
  // The row badge is a shortcut, not a toggle: it shows the manual-only view.
  const showManualFilter = useCallback(() => {
    onFilterChange({ ...filter, manual: 'manual' });
  }, [filter, onFilterChange]);

  // The path filter input is debounced (~250ms) so typing does not refetch
  // per keystroke; committed values drive the page fetch and the URL.
  const [pathDraft, setPathDraft] = useState(filter.path ?? '');
  const pathTimerRef = useRef<number | undefined>(undefined);
  const pathInputRef = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    // External filter changes (reset, badge, URL sync) resync the draft and
    // cancel any pending keystroke commit.
    window.clearTimeout(pathTimerRef.current);
    setPathDraft(filter.path ?? '');
  }, [filter.path]);
  useEffect(() => () => window.clearTimeout(pathTimerRef.current), []);
  const commitPath = useCallback(
    (value: string) => {
      window.clearTimeout(pathTimerRef.current);
      if ((filter.path ?? '') !== value) {
        onFilterChange({ ...filter, path: value });
      }
    },
    [filter, onFilterChange],
  );
  const scheduleCommitPath = (value: string) => {
    setPathDraft(value);
    window.clearTimeout(pathTimerRef.current);
    pathTimerRef.current = window.setTimeout(() => commitPath(value), 250);
  };
  const clearPathFilter = () => {
    setPathDraft('');
    commitPath('');
  };
  const filtersDefault = isDefaultFilter(filter);
  // Live provider work belongs next to the rows it fills in, not in the
  // header: one strip carries the metadata progress and the cache lookups
  // behind it, and disappears when there is nothing to report.
  const metadataTotal = metadataProgress?.total ?? 0;
  const metadataLabel = metadataProgressLabel(
    metadataProgress?.completed ?? 0,
    metadataTotal,
    cacheHits,
    cacheMisses,
  );
  const openSuggestion = useMemo(
    () => suggestions.find((s) => s.id === openId) ?? null,
    [suggestions, openId],
  );

  // The list is windowed with only the rows in and around the viewport
  // mounted, and the rows themselves are fetched a page at a time from the
  // service, so a batch of hundreds of thousands of suggestions never becomes
  // a full DOM tree or a single multi-megabyte payload.
  const virtualizer = useVirtualizer({
    count: suggestions.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    getItemKey: (index) => suggestions[index].id,
    overscan: 12,
  });

  // Fetch the next server page before the user reaches the end of the
  // loaded window.
  const handleScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < ROW_HEIGHT * 12) {
      onLoadMore();
    }
  };

  const clampDetailWidth = (width: number) => {
    const panes = panesRef.current;
    const max = panes ? Math.max(320, panes.clientWidth - 320 - 12) : 720;
    return Math.min(max, Math.max(280, width));
  };

  const onDividerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    e.preventDefault();
    dragRef.current = { startX: e.clientX, startWidth: detailWidth };
    e.currentTarget.setPointerCapture(e.pointerId);
  };

  const onDividerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    if (!drag) return;
    // The pane sits on the right, so dragging left widens it.
    const delta = e.clientX - drag.startX;
    setDetailWidth(clampDetailWidth(drag.startWidth - delta));
  };

  const endDrag = () => {
    dragRef.current = null;
  };

  const onDividerKey = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    e.preventDefault();
    const step = e.key === 'ArrowLeft' ? 24 : -24;
    setDetailWidth((w) => clampDetailWidth(w + step));
  };

  // The closed status filter takes the current choice's outcome colour too,
  // so it reads like the matching rows and its own options.
  const statusTone = filter.status === 'all' ? null : rowTone(filter.status);

  return (
    <main className="browse">
      <div className="toolbar-row">
        <div className="filters">
          <div className="filter-input-wrap">
            <input
              ref={pathInputRef}
              className="filter-input"
              type="text"
              aria-label="path filter"
              placeholder="filter by path"
              title="Match the source, destination, or duplicate-keeper path; case-insensitive substring"
              value={pathDraft}
              onChange={(e) => scheduleCommitPath(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === 'Escape') clearPathFilter();
              }}
            />
            {pathDraft !== '' && (
              <button
                type="button"
                className="filter-input-clear"
                aria-label="clear path filter"
                title="clear path filter"
                onClick={() => {
                  clearPathFilter();
                  pathInputRef.current?.focus();
                }}
              >
                <CloseIcon />
              </button>
            )}
          </div>
          <select
            className="filter-select"
            aria-label="type filter"
            value={filter.type}
            onChange={(e) =>
              onFilterChange({ ...filter, type: e.currentTarget.value as Filter['type'] })
            }
          >
            {TYPE_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
          <select
            className="filter-select"
            aria-label="asset kind filter"
            value={filter.kind}
            onChange={(e) => onFilterChange({ ...filter, kind: e.currentTarget.value })}
          >
            {kindOptions.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
          <select
            className={`filter-select${statusTone ? ` status-${statusTone}` : ''}`}
            aria-label="status filter"
            value={filter.status}
            onChange={(e) =>
              onFilterChange({ ...filter, status: e.currentTarget.value as Filter['status'] })
            }
          >
            {STATUS_OPTIONS.map((o) => {
              // The status choices carry the same outcome colours as the
              // suggestion rows, so the value reads the same in the list and
              // in the drop-down.
              const tone = o.value === 'all' ? null : rowTone(o.value);
              return (
                <option
                  key={o.value}
                  value={o.value}
                  className={tone === null ? undefined : `option-status-${tone}`}
                >
                  {o.label}
                </option>
              );
            })}
          </select>
          <select
            className="filter-select"
            aria-label="metadata filter"
            title="show only suggestions with saved manual overrides"
            value={filter.manual}
            onChange={(e) =>
              onFilterChange({ ...filter, manual: e.currentTarget.value as Filter['manual'] })
            }
          >
            {MANUAL_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
          <span className="count-chip" title="suggestions with saved manual metadata overrides">
            {manualCount} manual
          </span>
          <button
            type="button"
            className="filter-btn"
            disabled={filtersDefault}
            onClick={() => onFilterChange(ALL_FILTER)}
          >
            reset
          </button>
          <span className="count-chip">
            {suggestionCountLabel(totalSuggestions, filteredSuggestions, !filtersDefault)}
          </span>
        </div>
        <div className="toolbar">
          <button type="button" onClick={() => onSelectFiltered(filter)}>
            select filtered
          </button>
          <button type="button" onClick={() => onClearFiltered(filter)}>
            clear filtered
          </button>
          <span className="count-chip">{selectedCount} selected</span>
          <span className="sep" />
          <button
            type="button"
            className="caution"
            onClick={() => onRun()}
            disabled={executeDisabled}
            title={executeTitle}
          >
            execute
          </button>
          <button
            type="button"
            onClick={() => onExport()}
            disabled={exportDisabled}
            title={exportTitle}
          >
            export
          </button>
        </div>
      </div>
      {metadataLabel && (
        <div className="activity-strip" role="status">
          {metadataTotal > 0 && (
            <div className="activity-bar" aria-hidden="true">
              <div
                className="activity-fill"
                style={{ width: `${Math.round((((metadataProgress?.completed ?? 0) / metadataTotal) * 100))}%` }}
              />
            </div>
          )}
          <span className="activity-text">{metadataLabel}</span>
        </div>
      )}
      <div
        className="browse-panes"
        ref={panesRef}
        style={{ '--detail-width': `${detailWidth}px` } as CSSProperties}
      >
        <section className="list-pane" ref={scrollRef} onScroll={handleScroll}>
          {suggestions.length === 0 ? (
            <ul className="suggest-list">
              <li className="empty">no suggestions match this filter</li>
            </ul>
          ) : (
            <ul
              className="suggest-list"
              style={{ height: virtualizer.getTotalSize() }}
            >
              {virtualizer.getVirtualItems().map((v) => {
                const s = suggestions[v.index];
                return (
                  <li
                    key={s.id}
                    className={`list-row${openId === s.id ? ' row-open' : ''}`}
                    style={{
                      position: 'absolute',
                      top: 0,
                      left: 0,
                      width: '100%',
                      height: v.size,
                      transform: `translateY(${v.start}px)`,
                    }}
                  >
                    <SuggestionRow
                      suggestion={s}
                      root={root ?? null}
                      onToggle={onToggle}
                      onOpen={setOpenId}
                      onToggleManualFilter={showManualFilter}
                    />
                  </li>
                );
              })}
            </ul>
          )}
        </section>
        {openSuggestion && (
          <>
            <div
              className="pane-divider"
              role="separator"
              aria-orientation="vertical"
              aria-label="resize preview pane"
              tabIndex={0}
              onPointerDown={onDividerDown}
              onPointerMove={onDividerMove}
              onPointerUp={endDrag}
              onPointerCancel={endDrag}
              onKeyDown={onDividerKey}
            />
            <DetailPane
              key={openSuggestion.id}
              suggestion={openSuggestion}
              onClose={() => setOpenId(null)}
              onRefreshMetadata={onRefreshMetadata}
              onSetMetadata={onSetMetadata}
              onRebuild={onRebuild}
            />
          </>
        )}
      </div>
    </main>
  );
}
