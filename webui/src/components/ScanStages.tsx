import {
  scanProgressFraction,
  scanProgressLabel,
  scanProgressPercent,
} from '../lib/format';
import type { ScanStage } from '../types';

interface ScanStagesProps {
  /** Stage rows accumulated from `scan.progress` events; may be empty. */
  stages: ScanStage[];
  cancelDisabled: boolean;
  onCancel: () => void;
}

/**
 * The scanning view's stage list. Each `scan.progress` phase that has started
 * is one row: the meter on the left, a fixed-width track identical in every
 * row so the bars align, and the phase's description on the right. Completed
 * rows stay on screen frozen at their last reported counts with a full bar
 * and a `done` marker; only the active row - the last one - carries the
 * cancel button, since cancelling is about the work still running. Before the
 * first event arrives the list opens with one indeterminate placeholder row,
 * so a scan never starts without a moving meter (see DESIGN.md
 * "GUI via SPA").
 */
export function ScanStages({
  stages,
  cancelDisabled,
  onCancel,
}: ScanStagesProps) {
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
        <StageRow
          key={i}
          stage={stage}
          active={i === stages.length - 1}
          cancelDisabled={cancelDisabled}
          onCancel={onCancel}
        />
      ))}
    </div>
  );
}

function StageRow({
  stage,
  active,
  cancelDisabled,
  onCancel,
}: {
  stage: ScanStage;
  active: boolean;
  cancelDisabled: boolean;
  onCancel: () => void;
}) {
  // A completed row fills its bar and reads `done` even when the phase could
  // not know its total while running; the active row keeps the live fraction.
  const fraction = stage.complete ? 1 : scanProgressFraction(stage);
  const percent = stage.complete ? null : scanProgressPercent(stage);
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
        {stage.complete ? 'done' : percent != null ? `${percent}%` : ''}
      </span>
      <span className="scan-stage-label">{scanProgressLabel(stage)}</span>
      {active && (
        <div className="toolbar">
          <button
            type="button"
            onClick={onCancel}
            disabled={cancelDisabled}
            title={
              cancelDisabled ? 'nothing is in progress to cancel' : undefined
            }
          >
            cancel
          </button>
        </div>
      )}
    </div>
  );
}
