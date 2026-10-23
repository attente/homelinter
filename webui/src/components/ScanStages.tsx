import {
  scanProgressFraction,
  scanProgressLabel,
  scanProgressPercent,
} from '../lib/format';
import type { ScanStage } from '../types';

interface ScanStagesProps {
  /** Stage rows accumulated from `scan.progress` events; may be empty. */
  stages: ScanStage[];
}

/**
 * The scanning view's stage list. Each `scan.progress` phase that has started
 * is one row: the meter on the left, a fixed-width track identical in every
 * row so the bars align, and the phase's description on the right. Completed
 * rows stay on screen frozen at their last reported counts with a full bar
 * labelled `100%`. The cancel button is not part of the list: it lives once,
 * in the scanning header beside the `scanning ...` label, so no row needs a
 * reserved button slot and every row is the same height. Before the first
 * event arrives the list opens with one indeterminate placeholder row, so a
 * scan never starts without a moving meter (see DESIGN.md "GUI via SPA").
 */
export function ScanStages({ stages }: ScanStagesProps) {
  return (
    <div className="scan-stages">
      {stages.length === 0 && (
        <div className="scan-stage">
          <div
            className="scan-meter-bar indeterminate"
            role="progressbar"
            aria-label="scan progress"
          >
            <div className="scan-meter-fill" />
          </div>
          <span className="scan-stage-status" />
          <span className="scan-stage-label">starting</span>
        </div>
      )}
      {stages.map((stage, i) => (
        <StageRow key={i} stage={stage} />
      ))}
    </div>
  );
}

function StageRow({ stage }: { stage: ScanStage }) {
  // A completed row fills its bar and reads `100%` even when the phase could
  // not know its total while running; the active row keeps the live fraction.
  const fraction = stage.complete ? 1 : scanProgressFraction(stage);
  const percent = stage.complete ? 100 : scanProgressPercent(stage);
  return (
    <div className={stage.complete ? 'scan-stage done' : 'scan-stage'}>
      <div
        className={
          fraction == null ? 'scan-meter-bar indeterminate' : 'scan-meter-bar'
        }
        role="progressbar"
        aria-label={`scan progress: ${stage.phase}`}
        aria-valuemin={fraction == null ? undefined : 0}
        aria-valuemax={fraction == null ? undefined : 100}
        aria-valuenow={percent ?? undefined}
      >
        <div
          className="scan-meter-fill"
          style={fraction == null ? undefined : { width: `${fraction * 100}%` }}
        />
      </div>
      <span className="scan-stage-status">
        {percent != null ? `${percent}%` : ''}
      </span>
      <span className="scan-stage-label">{scanProgressLabel(stage)}</span>
    </div>
  );
}
