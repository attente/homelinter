import { scanProgressFraction, scanProgressPercent } from '../lib/format';
import type { ScanProgress } from '../types';

interface ScanMeterProps {
  /** Latest `scan.progress` event, or null before the first one arrives. */
  progress: ScanProgress | null;
}

/**
 * Progress meter for the scanning view, which is also the view's busy
 * indicator: a phase that knows its total up front (metadata, hashing, repos,
 * and the boundary reports) fills the bar to that fraction and labels it with
 * the percentage, while a phase that cannot know it (the discovery and symlinks
 * walks, which count entries visited until they stop, and the moment before the
 * first event arrives) bounces a segment back and forth along the track. So
 * every scan shows a moving meter, and none claims a completion it cannot know
 * (see DESIGN.md "GUI via SPA").
 */
export function ScanMeter({ progress }: ScanMeterProps) {
  // Before the first event there is no phase to report, so the meter starts
  // indeterminate rather than staying invisible.
  const event = progress ?? { scanned: 0, total: null };
  const fraction = scanProgressFraction(event);
  const percent = scanProgressPercent(event);
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
