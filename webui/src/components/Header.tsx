import { metadataProgressLabel, stateLabel } from '../lib/format';
import type { MetadataProgress } from '../types';

export interface HeaderCounts {
  suggestions: number;
  shown: number;
  selected: number;
  /** Suggestions with at least one saved manual metadata override. */
  manual: number;
}

interface HeaderProps {
  state: string;
  root: string | null;
  counts: HeaderCounts;
  metadataProgress?: MetadataProgress | null;
  /** Provider cache lookups for the current batch (0 until a provider
   * request is made; the pair is hidden while both are zero). */
  cacheHits: number;
  cacheMisses: number;
  busy: boolean;
  scanDisabled?: boolean;
  onNewScan: () => void;
  onConfig: () => void;
}

export function Header(props: HeaderProps) {
  const { state, root, counts, metadataProgress, cacheHits, cacheMisses, busy, scanDisabled, onNewScan, onConfig } = props;
  const badgeClass = state === 'ready' || state === 'done' ? 'state-ok' : state === 'failed' ? 'state-bad' : 'state-busy';
  // One label: metadata progress and the cache lookups behind it.
  const metadataLabel = metadataProgressLabel(
    metadataProgress?.completed ?? 0,
    metadataProgress?.total ?? 0,
    cacheHits,
    cacheMisses,
  );
  return (
    <header className="header">
      <span className="title">homelinter</span>
      <span className={badgeClass}>{stateLabel(state)}{busy ? '...' : ''}</span>
      <span className="root-label">{root ?? 'no scan root'}</span>
      <span className="counts">
        {counts.suggestions} suggestions, {counts.shown} shown, {counts.selected} selected, {counts.manual} with manual overrides
      </span>
      {metadataLabel && (
        <span className="counts metadata-progress">
          {metadataLabel}
        </span>
      )}
      <nav className="header-actions">
        <button
          type="button"
          onClick={onNewScan}
          disabled={scanDisabled}
          title={scanDisabled ? 'a scan or execution is in progress' : undefined}
        >
          scan
        </button>
        <button type="button" onClick={onConfig}>config</button>
      </nav>
    </header>
  );
}