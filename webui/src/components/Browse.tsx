import { useMemo, useState } from 'react';
import {
  ALL_FILTER,
  Filter,
  STATUS_CYCLE,
  TYPE_CYCLE,
  cycleNext,
  matchesFilter,
  uniqueKinds,
} from '../lib/filter';
import { rowText } from '../lib/format';
import type { Suggestion } from '../types';

export interface FilterState {
  type: 'all' | 'move' | 'delete';
  kind: 'all' | string;
  status: 'all' | Suggestion['status'];
}

type TabId = 'preview' | 'location' | 'metadata';

interface BrowseProps {
  suggestions: Suggestion[];
  actionsDisabled?: boolean;
  root?: string | null;
  onToggle: (id: string, select: boolean) => void;
  onSelectMany: (ids: string[]) => void;
  onClearSelection: () => void;
  onRun: () => void;
  onExport: () => void;
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

function DetailPane(props: { suggestion: Suggestion; onClose: () => void }) {
  const { suggestion, onClose } = props;
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
  if (lat !== null && lon !== null) tabs.push('location');
  if (rows.length > 0) tabs.push('metadata');
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
        <button type="button" className="tab-btn" onClick={onClose}>
          close
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
              <p className="detail-path">
                <span className="detail-label">from: </span>
                {suggestion.target}
              </p>
              <p className="detail-path">
                <span className="detail-label">to: </span>
                {suggestion.new_path ?? '?'}
              </p>
            </>
          ) : (
            <>
              <p className="detail-action">delete</p>
              <p className="detail-path">
                <span className="detail-label">target: </span>
                {suggestion.target}
              </p>
              {typeof m.duplicate_of === 'string' && m.duplicate_of.length > 0 && (
                <p className="detail-path">
                  <span className="detail-label">original: </span>
                  {m.duplicate_of}
                </p>
              )}
            </>
          )}
          <p className="detail-reason">{suggestion.reason}</p>
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
      )}
    </aside>
  );
}

export function Browse(props: BrowseProps) {
  const { suggestions, actionsDisabled, root, onToggle, onSelectMany, onClearSelection, onRun, onExport } = props;
  const [filter, setFilter] = useState<Filter>(ALL_FILTER);
  const [openId, setOpenId] = useState<string | null>(null);

  const kinds = useMemo(() => uniqueKinds(suggestions), [suggestions]);
  const kindCycle = useMemo(() => ['all', ...kinds], [kinds]);
  const filtered = useMemo(
    () => suggestions.filter((s) => matchesFilter(s, filter)),
    [suggestions, filter],
  );
  const openSuggestion = useMemo(
    () => suggestions.find((s) => s.id === openId) ?? null,
    [suggestions, openId],
  );

  return (
    <main className="browse">
      <div className="toolbar-row">
        <div className="filters">
          <button
            type="button"
            className="filter-btn"
            onClick={() => setFilter((f) => ({ ...f, type: cycleNext(TYPE_CYCLE, f.type) }))}
          >
            t:{filter.type}
          </button>
          <button
            type="button"
            className="filter-btn"
            onClick={() => setFilter((f) => ({ ...f, kind: cycleNext(kindCycle, f.kind) }))}
          >
            k:{filter.kind}
          </button>
          <button
            type="button"
            className="filter-btn"
            onClick={() => setFilter((f) => ({ ...f, status: cycleNext(STATUS_CYCLE, f.status) }))}
          >
            u:{filter.status}
          </button>
          <button type="button" className="filter-btn" onClick={() => setFilter(ALL_FILTER)}>
            clear
          </button>
        </div>
        <div className="toolbar">
          <button type="button" onClick={() => onSelectMany(filtered.map((s) => s.id))}>
            select all
          </button>
          <button type="button" onClick={() => onClearSelection()}>
            select none
          </button>
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
      <div className="browse-panes">
        <section className="list-pane">
          <ul className="suggest-list">
            {filtered.map((s) => (
              <li key={s.id} className={`list-row${openId === s.id ? ' row-open' : ''}`}>
                <input
                  type="checkbox"
                  className="row-check"
                  checked={s.selected}
                  onChange={() => onToggle(s.id, !s.selected)}
                  aria-label="select"
                />
                <button
                  type="button"
                  className="row-hit"
                  onClick={() => setOpenId(s.id)}
                >
                  {rowText(s, root)}
                </button>
              </li>
            ))}
            {filtered.length === 0 && <li className="empty">no suggestions match this filter</li>}
          </ul>
        </section>
        {openSuggestion && (
          <DetailPane
            key={openSuggestion.id}
            suggestion={openSuggestion}
            onClose={() => setOpenId(null)}
          />
        )}
      </div>
    </main>
  );
}
