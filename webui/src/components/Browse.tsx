import { useMemo, useState } from 'react';
import { detailLines } from '../lib/detail';
import {
  ALL_FILTER,
  Filter,
  STATUS_CYCLE,
  TYPE_CYCLE,
  cycleNext,
  matchesFilter,
  uniqueKinds,
} from '../lib/filter';
import { rowLabel } from '../lib/format';
import type { Suggestion } from '../types';

export interface FilterState {
  type: 'all' | 'move' | 'delete';
  kind: 'all' | string;
  status: 'all' | Suggestion['status'];
}

interface BrowseProps {
  suggestions: Suggestion[];
  actionsDisabled?: boolean;
  onToggle: (id: string, select: boolean) => void;
  onSelectMany: (ids: string[]) => void;
  onClearSelection: () => void;
  onRun: () => void;
  onExport: () => void;
}

export function Browse(props: BrowseProps) {
  const { suggestions, actionsDisabled, onToggle, onSelectMany, onClearSelection, onRun, onExport } = props;
  const [filter, setFilter] = useState<Filter>(ALL_FILTER);
  const [cursor, setCursor] = useState(0);

  const kinds = useMemo(() => uniqueKinds(suggestions), [suggestions]);
  const kindCycle = useMemo(() => ['all', ...kinds], [kinds]);
  const filtered = useMemo(
    () => suggestions.filter((s) => matchesFilter(s, filter)),
    [suggestions, filter],
  );
  const selectedIds = useMemo(
    () => new Set(suggestions.filter((s) => s.selected).map((s) => s.id)),
    [suggestions],
  );

  const shown = cursor >= 0 && cursor < filtered.length ? filtered[cursor] : undefined;

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
            {filtered.map((s, i) => (
              <li key={s.id} className={i === cursor ? 'list-row current' : 'list-row'}>
                <button
                  type="button"
                  className="row-hit"
                  onClick={() => {
                    setCursor(i);
                    onToggle(s.id, !selectedIds.has(s.id));
                  }}
                >
                  {rowLabel(s)}
                </button>
              </li>
            ))}
            {filtered.length === 0 && <li className="empty">no suggestions match this filter</li>}
          </ul>
        </section>
        <section className="detail-pane">
          {shown ? <Detail suggestion={shown} /> : <p className="empty">no suggestion selected</p>}
        </section>
      </div>
    </main>
  );
}

function Detail({ suggestion }: { suggestion: Suggestion }) {
  const lines = detailLines(suggestion);
  return (
    <dl className="detail">
      {lines.map(([k, v]) => (
        <div key={k} className="detail-row">
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}