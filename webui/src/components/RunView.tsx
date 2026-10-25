import { useEffect, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import {
  actionText,
  countsSummaryParts,
  formatBytes,
  stateLabel,
  statusLabel,
} from '../lib/format';
import { isFinal, type RunState } from '../lib/run';
import { useEscapeClose } from '../lib/modal';
import type { ExecutionReport } from '../types';
import { CloseIcon } from './CloseIcon';
import { CopyablePath } from './CopyablePath';
import { DownloadIcon } from './DownloadIcon';

interface RunViewProps {
  state: string;
  /** Live state of the run: every requested suggestion with its action
   *  sentence and status. Null only if the view is entered without a run. */
  run: RunState | null;
  report: ExecutionReport | null;
  error: string | null;
  cancelDisabled?: boolean;
  onCancel: () => void;
  onDone: () => void;
  /** Download the finished report to a location the user chooses. */
  onDownload: () => void;
}

const ROW_HEIGHT = 26;

/**
 * The execution modal. It fills almost the whole window like the full-screen
 * preview. The header carries the session state, one progress meter over the
 * suggestions that have finished, and cancel, with the summary counts after
 * the disabled cancel once the report is in and the close icon pushed to the
 * top-right corner. Below it, one row per suggestion in the run - in
 * execution order, showing the action it performs (never its id) and a
 * status that updates as the run proceeds. A download icon floats at the
 * top-right corner of the status list, and a partial run or run error shows
 * as a plain status line below the list. There is no console log: this list
 * is the status.
 *
 * The view is a modal over the suggestion browser: the close icon, a click
 * on the backdrop, and Escape all run the same exit, so leaving mid-run just
 * hides the view and the execution carries on in the background.
 */
export function RunView(props: RunViewProps) {
  const {
    state,
    run,
    report,
    error,
    cancelDisabled,
    onCancel,
    onDone,
    onDownload,
  } = props;
  const items = run?.items ?? [];
  const done = run?.done ?? 0;
  const activeIndex = run?.activeIndex ?? -1;

  const scrollRef = useRef<HTMLDivElement | null>(null);
  // Follow the active suggestion like a console, but only while the user has
  // not scrolled away from the bottom of the list.
  const followRef = useRef(true);
  const followedIndexRef = useRef(-1);

  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    getItemKey: (index) => items[index].id,
    overscan: 12,
  });

  useEffect(() => {
    if (activeIndex < 0 || !followRef.current) return;
    if (followedIndexRef.current === activeIndex) return;
    followedIndexRef.current = activeIndex;
    virtualizer.scrollToIndex(activeIndex, { align: 'auto' });
  }, [activeIndex, virtualizer]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const follow =
      el.scrollHeight - el.scrollTop - el.clientHeight < ROW_HEIGHT * 4;
    if (follow && !followRef.current && activeIndex >= 0) {
      followedIndexRef.current = activeIndex;
      virtualizer.scrollToIndex(activeIndex, { align: 'auto' });
    }
    followRef.current = follow;
  };

  const percent =
    items.length === 0 ? 100 : Math.round((done / items.length) * 100);

  useEscapeClose(true, onDone);

  return (
    <div
      className="modal-backdrop run-backdrop"
      role="presentation"
      onClick={onDone}
    >
      <div
        className="modal run-modal"
        role="dialog"
        aria-modal="true"
        aria-label="execution"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="run-status">
          <span className="state-busy">{stateLabel(state)}</span>
          <div
            className="scan-meter-bar"
            role="progressbar"
            aria-label="execution progress"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={percent}
          >
            <div
              className="scan-meter-fill"
              style={{ width: `${percent}%` }}
            />
          </div>
          <span className="run-progress-count">
            {done}/{items.length}
          </span>
          <div className="toolbar">
            <button
              type="button"
              onClick={onCancel}
              disabled={cancelDisabled}
              title={cancelDisabled ? 'nothing is in progress to cancel' : undefined}
            >
              cancel
            </button>
            {report && (
              <ReportSummary run={run} reclaimedBytes={report.reclaimed_bytes} />
            )}
          </div>
          <button
            type="button"
            className="close-btn"
            aria-label="close execution view"
            title="close execution view"
            onClick={onDone}
          >
            <CloseIcon />
          </button>
        </div>
        <div className="run-list-wrap">
          <div className="run-items" ref={scrollRef} onScroll={onScroll}>
            {items.length === 0 ? (
              <div className="run-empty">no suggestions in this run</div>
            ) : (
              <ol className="run-list" style={{ height: virtualizer.getTotalSize() }}>
                {virtualizer.getVirtualItems().map((v) => {
                  const item = items[v.index];
                  // The active suggestion is the one executing or next in line:
                  // the service runs one suggestion at a time, so the first row
                  // without a final result is the live one.
                  const status =
                    v.index === activeIndex && !isFinal(item.status)
                      ? 'running'
                      : item.status;
                  return (
                    <li
                      key={item.id}
                      className={`run-item status-${status}`}
                      style={{
                        position: 'absolute',
                        top: 0,
                        left: 0,
                        width: '100%',
                        height: v.size,
                        transform: `translateY(${v.start}px)`,
                      }}
                    >
                      <span className="run-item-status">{statusLabel(status)}</span>
                      <span className="run-item-label" title={actionText(item.parts)}>
                        {item.parts.map((p, i) =>
                          p.kind === 'path' ? (
                            <CopyablePath
                              key={i}
                              value={p.path ?? p.text}
                              display={p.text}
                              className="run-path"
                              buttonClassName="run-path-copy"
                            />
                          ) : (
                            p.text
                          ),
                        )}
                      </span>
                    </li>
                  );
                })}
              </ol>
            )}
          </div>
          {report && (
            <button
              type="button"
              className="close-btn run-download"
              aria-label="download report"
              title="download report to a file"
              onClick={onDownload}
            >
              <DownloadIcon />
            </button>
          )}
        </div>
        {report?.partial && (
          <p className="note warn">
            partial: some operations completed without finishing source cleanup or dependent repairs
          </p>
        )}
        {error && <p className="error">{error}</p>}
      </div>
    </div>
  );
}

/** The run's final counts, shown in the status header immediately after the
 *  disabled cancel button once the report is in: the same words the
 *  downloaded report and the TUI use, each count coloured by its outcome's
 *  goodness (the colours its rows use on the list). */
function ReportSummary({
  run,
  reclaimedBytes,
}: {
  run: RunState | null;
  reclaimedBytes: number;
}) {
  const counts = { executed: 0, partial: 0, failed: 0, skipped: 0 };
  if (run) {
    for (const item of run.items) {
      if (isFinal(item.status)) counts[item.status] += 1;
    }
  }
  return (
    <span className="run-summary">
      {countsSummaryParts(counts).map((part) => (
        <span key={part.label} className={`run-summary-part part-${part.tone}`}>
          {part.label}
        </span>
      ))}
      {reclaimedBytes > 0 && (
        <span className="run-summary-part part-ok">
          reclaimed {formatBytes(reclaimedBytes)}
        </span>
      )}
    </span>
  );
}
