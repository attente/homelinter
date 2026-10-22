import { scanProgressFraction, scanProgressPercent } from '../lib/format';
import type { ScanProgress } from '../types';

interface ScanMeterProps {
  progress: ScanProgress | null;
}

/**
 * Progress meter for the scanning view. A phase that knows its total up front
 * (metadata, hashing, repos, and the boundary reports) fills the bar to that
 * fraction and labels it with the percentage; a phase that cannot know it (the
 * discovery and symlinks walks, which count entries visited until they stop)
 * shows a sliding segment instead, so the meter still moves and never claims a
 * completion it cannot know (see DESIGN.md "GUI via SPA").
 */
export function ScanMeter({ progress }: ScanMeterProps) {
  if (!progress) return null;
  const fraction = scanProgressFraction(progress);
  const percent = scanProgressPercent(progress);
  return (
    <div className="scan-meter">
      <div
        className={
          fraction == null ? 'scan-meter-bar indeterminate' : 'scan-meter-bar'
        }
        role="progressbar"
        aria-label="scan progress"
        aria-valuemin={fraction == null ? undefined : 0}
        aria-valuemax={fraction == null ? undefined : 100}
        aria-valuenow={percent ?? undefined}
      >
        <div
          className="scan-meter-fill"
          style={percent == null ? undefined : { width: `${percent}%` }}
        />
      </div>
      {percent != null && <span className="scan-meter-percent">{percent}%</span>}
    </div>
  );
}
