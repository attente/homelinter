import { stateLabel } from '../lib/format';
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
  busy: boolean;
  scanDisabled?: boolean;
  onNewScan: () => void;
  onConfig: () => void;
}

export function Header(props: HeaderProps) {
  const { state, root, counts, metadataProgress, busy, scanDisabled, onNewScan, onConfig } = props;
  const badgeClass = state === 'ready' || state === 'done' ? 'state-ok' : state === 'failed' ? 'state-bad' : 'state-busy';
  return (
    <header className="header">
      <span className="title">homelinter</span>
      <span className={badgeClass}>{stateLabel(state)}{busy ? '...' : ''}</span>
      <span className="root-label">{root ?? 'no scan root'}</span>
      <span className="counts">
        {counts.suggestions} suggestions, {counts.shown} shown, {counts.selected} selected, {counts.manual} with manual overrides
      </span>
      {metadataProgress && metadataProgress.total > 0 && (
        <span className="counts metadata-progress">
          metadata: {metadataProgress.completed}/{metadataProgress.total}
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