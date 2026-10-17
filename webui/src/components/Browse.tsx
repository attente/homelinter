import { memo, useEffect, useMemo, useRef, useState } from 'react';
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
} from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import {
  ALL_FILTER,
  Filter,
  STATUS_CYCLE,
  TYPE_CYCLE,
  cycleNext,
} from '../lib/filter';
import { rowText } from '../lib/format';
import type { Suggestion } from '../types';
import { CloseIcon } from './CloseIcon';
import { CheckIcon, CopyIcon } from './CopyIcon';

export interface FilterState {
  type: 'all' | 'move' | 'delete';
  kind: 'all' | string;
  status: 'all' | Suggestion['status'];
}

type TabId = 'preview' | 'location' | 'metadata';

interface BrowseProps {
  suggestions: Suggestion[];
  /** Server total for the active filter; may exceed the loaded window. */
  total: number;
  /** Complete set of asset kinds for the batch (from the report summary). */
  kinds: string[];
  filter: Filter;
  actionsDisabled?: boolean;
  root?: string | null;
  onFilterChange: (filter: Filter) => void;
  onLoadMore: () => void;
  onToggle: (id: string, select: boolean) => void;
  onSelectAll: (filter: Filter) => void;
  onClearSelection: () => void;
  onRun: () => void;
  onExport: () => void;
  onRefreshMetadata: (id: string) => void;
  onSetMetadata: (
    id: string,
    edit: { location: string; subject: string; author: string },
  ) => void;
  onRebuild: (id: string) => void;
}

// The service falls back to a bare "lat,lng" string when reverse geocoding
// fails, so a bare decimal pair is not a place name.
const COORDS_ONLY = /^\s*-?\d+(\.\d+)?\s*,\s*-?\d+(\.\d+)?\s*$/;

function previewKind(path: string): 'image' | 'video' | 'audio' | 'pdf' | 'text' | null {
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
function RefreshMetadataButton(props: {
  suggestion: Suggestion;
  onRefresh: (id: string) => void;
}) {
  const { suggestion, onRefresh } = props;
  if (suggestion.type !== 'move') return null;
  const pending = suggestion.metadata_pending;
  return (
    <button
      type="button"
      className="meta-refresh-btn"
      disabled={pending}
      title="discard the cached provider results and ask again"
      onClick={() => onRefresh(suggestion.id)}
    >
      {pending ? 're-requesting...' : 're-request metadata'}
    </button>
  );
}

/** Re-apply the effective metadata and rebuild this move's destination without
 *  a scan or a provider call. */
function RebuildButton(props: {
  suggestion: Suggestion;
  onRebuild: (id: string) => void;
}) {
  const { suggestion, onRebuild } = props;
  if (suggestion.type !== 'move') return null;
  return (
    <button
      type="button"
      className="meta-refresh-btn"
      title="re-apply saved metadata edits and rebuild the destination"
      onClick={() => onRebuild(suggestion.id)}
    >
      rebuild
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
function PathRow(props: { label: string; value: string }) {
  const { label, value } = props;
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
    </p>
  );
}

/** Manual metadata editor: the three values that shape a move's destination
 *  (`%l`, `%s`/`%t`, `%a`). Saving replaces all three overrides for this
 *  content, and the service persists them keyed by content identity. The
 *  fields start from the effective metadata (a manual edit, a provider
 *  value, or embedded metadata) so a re-request is a starting point, not a
 *  reset. */
function MetadataEditor(props: {
  suggestion: Suggestion;
  onSave: (
    id: string,
    edit: { location: string; subject: string; author: string },
  ) => void;
}) {
  const { suggestion, onSave } = props;
  const m = suggestion.metadata ?? {};
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
        <span className="field-label">Location (%l)</span>
        <input
          value={location}
          onChange={(e) => setLocation(e.target.value)}
          placeholder="leave blank to use geocoder or embedded GPS"
        />
      </label>
      <label className="field">
        <span className="field-label">Subject (%s, %t)</span>
        <input
          value={subject}
          onChange={(e) => setSubject(e.target.value)}
          placeholder="leave blank to use embedded title or a description"
        />
      </label>
      <label className="field">
        <span className="field-label">Author (%a)</span>
        <input
          value={author}
          onChange={(e) => setAuthor(e.target.value)}
          placeholder="leave blank to use embedded or inferred author"
        />
      </label>
      <button type="submit" className="meta-save-btn">
        save metadata
      </button>
    </form>
  );
}

function DetailPane(props: {
  suggestion: Suggestion;
  onClose: () => void;
  onRefreshMetadata: (id: string) => void;
  onSetMetadata: (
    id: string,
    edit: { location: string; subject: string; author: string },
  ) => void;
  onRebuild: (id: string) => void;
}) {
  const { suggestion, onClose, onRefreshMetadata, onSetMetadata, onRebuild } = props;
  const [tab, setTab] = useState<TabId>('preview');

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
          {previewKind(suggestion.target) === 'image' && (
            <img
              className="file-preview-image"
              src={`/api/v1/suggestions/${encodeURIComponent(suggestion.id)}/preview`}
              alt="file preview"
            />
          )}
          {previewKind(suggestion.target) === 'video' && (
            <video
              className="file-preview-media"
              controls
              preload="metadata"
              src={`/api/v1/suggestions/${encodeURIComponent(suggestion.id)}/preview`}
            />
          )}
          {previewKind(suggestion.target) === 'audio' && (
            <audio
              className="file-preview-audio"
              controls
              preload="metadata"
              src={`/api/v1/suggestions/${encodeURIComponent(suggestion.id)}/preview`}
            />
          )}
          {previewKind(suggestion.target) === 'pdf' && (
            <iframe
              className="file-preview-pdf"
              src={`/api/v1/suggestions/${encodeURIComponent(suggestion.id)}/preview`}
              title="PDF preview"
            />
          )}
          {previewKind(suggestion.target) === 'text' && (
            <iframe
              className="file-preview-text"
              src={`/api/v1/suggestions/${encodeURIComponent(suggestion.id)}/preview`}
              title="text file preview"
            />
          )}
          {suggestion.type === 'move' ? (
            <>
              <p className="detail-action">move</p>
              <PathRow label="from:" value={suggestion.target} />
              <PathRow label="to:" value={suggestion.new_path ?? '?'} />
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
          <RefreshMetadataButton
            suggestion={suggestion}
            onRefresh={onRefreshMetadata}
          />
          <RebuildButton suggestion={suggestion} onRebuild={onRebuild} />
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
          <MetadataEditor suggestion={suggestion} onSave={onSetMetadata} />
          <RefreshMetadataButton
            suggestion={suggestion}
            onRefresh={onRefreshMetadata}
          />
          <RebuildButton suggestion={suggestion} onRebuild={onRebuild} />
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
}

/** One list row. Memoized so a checkbox toggle or a single provider-metadata
 *  update only reconciles the rows whose props actually changed. */
const SuggestionRow = memo(function SuggestionRow({
  suggestion,
  root,
  onToggle,
  onOpen,
}: SuggestionRowProps) {
  return (
    <>
      <input
        type="checkbox"
        className="row-check"
        checked={suggestion.selected}
        onChange={() => onToggle(suggestion.id, !suggestion.selected)}
        aria-label="select"
      />
      <button
        type="button"
        className="row-hit"
        onClick={() => onOpen(suggestion.id)}
      >
        {suggestion.metadata_pending && <span className="spinner" aria-hidden="true" />}
        {rowText(suggestion, root)}
      </button>
    </>
  );
});

export function Browse(props: BrowseProps) {
  const { suggestions, total, kinds, filter, actionsDisabled, root, onFilterChange, onLoadMore, onToggle, onSelectAll, onClearSelection, onRun, onExport, onRefreshMetadata, onSetMetadata, onRebuild } = props;
  const [openId, setOpenId] = useState<string | null>(null);
  const scrollRef = useRef<HTMLElement | null>(null);
  const panesRef = useRef<HTMLDivElement | null>(null);
  // The detail pane width is driven by a draggable divider and clamped so
  // neither pane can be dragged below a usable minimum.
  const [detailWidth, setDetailWidth] = useState(360);
  const dragRef = useRef<{ startX: number; startWidth: number } | null>(null);

  const kindCycle = useMemo(() => ['all', ...kinds], [kinds]);
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

  return (
    <main className="browse">
      <div className="toolbar-row">
        <div className="filters">
          <button
            type="button"
            className="filter-btn"
            onClick={() => onFilterChange({ ...filter, type: cycleNext(TYPE_CYCLE, filter.type) })}
          >
            t:{filter.type}
          </button>
          <button
            type="button"
            className="filter-btn"
            onClick={() => onFilterChange({ ...filter, kind: cycleNext(kindCycle, filter.kind) })}
          >
            k:{filter.kind}
          </button>
          <button
            type="button"
            className="filter-btn"
            onClick={() => onFilterChange({ ...filter, status: cycleNext(STATUS_CYCLE, filter.status) })}
          >
            u:{filter.status}
          </button>
          <button type="button" className="filter-btn" onClick={() => onFilterChange(ALL_FILTER)}>
            clear
          </button>
        </div>
        <div className="toolbar">
          <button type="button" onClick={() => onSelectAll(filter)}>
            select all
          </button>
          <button type="button" onClick={() => onClearSelection()}>
            select none
          </button>
          <span className="sep" />
          <span className="list-count">
            {suggestions.length}/{total}
          </span>
          <span className="sep" />
          <button
            type="button"
            onClick={() => onRun()}
            disabled={actionsDisabled}
            title={actionsDisabled ? 'execute is only available from a ready or done session' : undefined}
          >
            execute
          </button>
          <button
            type="button"
            onClick={() => onExport()}
            disabled={actionsDisabled}
            title={actionsDisabled ? 'export is only available from a ready or done session' : undefined}
          >
            export
          </button>
        </div>
      </div>
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
