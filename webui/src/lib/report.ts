// Text form of the execution report, downloaded from the run view.
//
// The rendered document is ordered by importance: the summary counts come
// first, then the errors that occurred, then every action in execution order
// with the notable per-operation outcomes. The same file can be produced while
// the run is still live, but the download is offered only once the final
// report is in, so statuses and counts are authoritative.

import type {
  EntryResult,
  ExecutionReport,
  OperationOutcome,
  ReportEntry,
} from '../types';
import { actionText, type ReportCounts } from './format';
import { isFinal, type RunItemStatus, type RunState } from './run';

export interface ExecutionReportInput {
  report: ExecutionReport;
  /** The run view's model, which holds every requested suggestion (including
   *  ones the report omits as skipped). Null when the view was entered
   *  without a live run. */
  run: RunState | null;
  /** Session-level execution error, when the run failed. */
  error?: string | null;
  generatedAt?: Date;
}

/** One requested suggestion in the report document: the action sentence with
 *  absolute paths when the operation outcomes provide them, its result, and
 *  the report's per-operation outcomes. */
export interface ReportAction {
  id: string;
  label: string;
  status: EntryResult;
  operations: OperationOutcome[];
}

/** The report's actions in execution order. The run model is authoritative for
 *  the complete set of requested suggestions; report entries missing from it
 *  (which the view would not normally have) are appended. */
export function reportActions(
  report: ExecutionReport,
  run: RunState | null,
): ReportAction[] {
  const entries = new Map(
    report.entries.map((e) => [e.suggestion_id, e] as const),
  );
  const actions: ReportAction[] = [];
  const seen = new Set<string>();
  if (run) {
    for (const item of run.items) {
      seen.add(item.id);
      actions.push(
        actionFor(
          item.id,
          actionText(item.parts),
          finalStatus(item.status),
          entries.get(item.id),
        ),
      );
    }
  }
  for (const entry of report.entries) {
    if (seen.has(entry.suggestion_id)) continue;
    actions.push(actionFor(entry.suggestion_id, null, entry.result, entry));
  }
  return actions;
}

function finalStatus(status: RunItemStatus): EntryResult {
  return isFinal(status) ? status : 'skipped';
}

function actionFor(
  id: string,
  fallbackLabel: string | null,
  status: EntryResult,
  entry: ReportEntry | undefined,
): ReportAction {
  const operations = entry?.operations ?? [];
  return {
    id,
    label: operationsLabel(operations) ?? fallbackLabel ?? id,
    status,
    operations,
  };
}

/** `move <source> to <destination>` or `delete <source>` from the parent
 *  operation, so the report carries the full absolute paths it recorded.
 *  Dependent repairs are not parent actions, so they never supply it. */
function operationsLabel(operations: OperationOutcome[]): string | null {
  const parent =
    operations.find((o) => o.operation === 'move') ??
    operations.find((o) => o.operation === 'delete');
  if (!parent || !parent.source) return null;
  if (parent.operation === 'move') {
    return parent.destination
      ? `move ${parent.source} to ${parent.destination}`
      : `move ${parent.source}`;
  }
  return `delete ${parent.source}`;
}

/** An action counts as an error when it did not fully finish, or when one of
 *  its operations failed, was skipped, or carried a detail (a completed move
 *  with a detail is the published partial cross-filesystem completion). */
function isErrorAction(action: ReportAction): boolean {
  if (action.status === 'failed' || action.status === 'partial') return true;
  return action.operations.some(
    (op) => op.status !== 'completed' || !!op.detail,
  );
}

/** The operation outcomes worth printing below an action: dependent repairs,
 *  anything that did not complete, and any operation carrying a detail. A
 *  plain completed parent operation repeats the action line, so it is left
 *  out. */
function notableOps(action: ReportAction): OperationOutcome[] {
  return action.operations.filter(
    (op) =>
      op.operation === 'repair_symlink' ||
      op.status !== 'completed' ||
      !!op.detail,
  );
}

function operationLine(op: OperationOutcome): string {
  const parts = [`${op.operation}: ${op.status}`];
  if (op.source && op.destination) {
    parts.push(`${op.source} -> ${op.destination}`);
  } else if (op.source ?? op.destination) {
    parts.push((op.source ?? op.destination) as string);
  }
  const line = `    ${parts.join(' ')}`;
  return op.detail ? `${line} - ${op.detail}` : line;
}

function actionLine(action: ReportAction): string {
  return `  [${action.status}] ${action.label}`;
}

function countActions(actions: ReportAction[]): ReportCounts {
  const counts: ReportCounts = { executed: 0, partial: 0, failed: 0, skipped: 0 };
  for (const action of actions) counts[action.status] += 1;
  return counts;
}

/**
 * The downloadable report as plain text: a title, the summary counts, the
 * run's context, the errors, and then every action with its notable
 * operation outcomes. ASCII only, matching the TUI's rendering conventions.
 */
export function executionReportText(input: ExecutionReportInput): string {
  const { report, run, error } = input;
  const at = input.generatedAt ?? new Date();
  const actions = reportActions(report, run);
  const counts = countActions(actions);
  const errors = actions.filter(isErrorAction);

  const lines: string[] = [];
  lines.push('homelinter execution report');
  lines.push('');
  lines.push(`summary (${actions.length} ${plural('action', actions.length)})`);
  for (const key of ['executed', 'partial', 'failed', 'skipped'] as const) {
    lines.push(`  ${key.padEnd(8)} ${counts[key]}`);
  }
  lines.push('');
  if (error) {
    lines.push(`run error: ${error}`);
  }
  lines.push(`scan root: ${report.scan_root}`);
  lines.push(`platform: ${report.platform}`);
  lines.push(`generated: ${at.toISOString()}`);
  lines.push(
    report.partial
      ? 'partial: yes (some actions completed without finishing source cleanup or dependent repairs)'
      : 'partial: no',
  );

  if (errors.length > 0) {
    lines.push('');
    lines.push(`errors (${errors.length})`);
    for (const action of errors) {
      lines.push(actionLine(action));
      for (const op of notableOps(action)) lines.push(operationLine(op));
    }
  }

  lines.push('');
  lines.push(`actions (${actions.length})`);
  for (const action of actions) {
    lines.push(actionLine(action));
    const ops = notableOps(action);
    for (const op of ops) lines.push(operationLine(op));
    if (action.status === 'skipped' && action.operations.length === 0) {
      lines.push('    no operations recorded');
    }
  }

  return `${lines.join('\n')}\n`;
}

function plural(word: string, count: number): string {
  return count === 1 ? word : `${word}s`;
}

/**
 * Suggested filename for the downloaded report: the UTC date and time
 * stamped in a filesystem-safe form, e.g.
 * `homelinter-execution-report-2025-03-05T14-02-11.txt`.
 */
export function executionReportFilename(at: Date = new Date()): string {
  const stamp = at.toISOString().slice(0, 19).replace(/[:T]/g, '-');
  return `homelinter-execution-report-${stamp}.txt`;
}
