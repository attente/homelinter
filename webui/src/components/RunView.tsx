import { stateLabel } from '../lib/format';
import type { EntryResult, ExecutionReport, ReportEntry, ScanProgress } from '../types';

interface RunViewProps {
  state: string;
  progress: ScanProgress | null;
  log: string[];
  report: ExecutionReport | null;
  error: string | null;
  onCancel: () => void;
  onDone: () => void;
}

const TOTAL_RESULTS: EntryResult[] = ['executed', 'failed', 'skipped'];

export function RunView(props: RunViewProps) {
  const { state, progress, log, report, error, onCancel, onDone } = props;
  const spinner = '|/-\\'[Math.floor(Date.now() / 120) % 4];

  return (
    <main className="run-view">
      <div className="run-status">
        <span className="state-busy">{stateLabel(state)} {spinner}</span>
        {progress && (
          <span className="progress-line">
            {progress.phase}, scanned={progress.scanned}, elapsed={progress.elapsed_ms}ms
          </span>
        )}
        <div className="toolbar">
          <button type="button" onClick={onCancel}>cancel</button>
        </div>
      </div>
      <pre className="log">
        {log.map((line, i) => (
          <div key={i}>{line}</div>
        ))}
        {log.length === 0 && <div className="empty">waiting for operations...</div>}
      </pre>
      {report && (
        <section className="report">
          <h2>Run finished</h2>
          {report.partial && <p className="note warn">partial: some dependent repairs failed</p>}
          {error && <p className="error">{error}</p>}
          <RunReport report={report} onDone={onDone} />
        </section>
      )}
      {!report && error && <p className="error">{error}</p>}
    </main>
  );
}

function RunReport({ report, onDone }: { report: ExecutionReport; onDone: () => void }) {
  const counts: Record<EntryResult, number> = { executed: 0, failed: 0, skipped: 0 };
  for (const e of report.entries) {
    counts[e.result] += 1;
  }
  const summary = TOTAL_RESULTS
    .filter((k) => counts[k] > 0)
    .map((k) => `${k}=${counts[k]}`)
    .join('  ');
  return (
    <>
      <p className="report-summary">{summary || 'no entries executed'}</p>
      <ul className="report-entries">
        {report.entries.map((e) => (
          <li key={e.suggestion_id}>{resultLine(e)}</li>
        ))}
      </ul>
      <div className="toolbar">
        <button type="button" onClick={onDone}>back to suggestions</button>
      </div>
    </>
  );
}

function resultLine(e: ReportEntry): string {
  return `[${e.result}] ${e.suggestion_id}`;
}
