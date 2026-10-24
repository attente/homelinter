// Run-view model: the complete set of suggestions a run will execute, each
// with its action sentence and a live status folded from the `execution.op`
// events and the final execution report.

import type { EntryResult, ExecutionOp, ReportEntry, Suggestion } from '../types';
import { actionLabelParts, type ActionPart } from './format';

export type RunItemStatus = 'pending' | 'running' | EntryResult;

/** One operation outcome streamed for a suggestion. `detail` carries the
 *  operation's own note; a non-null detail on a completed parent move marks
 *  the partial cross-filesystem completion (see `deriveResult`). */
export interface RunItemOp {
  operation: string;
  status: string;
  detail?: string | null;
}

export interface RunItem {
  id: string;
  /** Action sentence as display parts in the past tense (`moved`, `deleted`):
   *  the connecting words stay in the UI typeface, the paths render in the
   *  monospace stack. For a suggestion whose record is not fetched yet, the
   *  single part is its id. */
  parts: ActionPart[];
  status: RunItemStatus;
  /** Operations streamed so far; drives the derived result when the run
   *  moves past this suggestion. */
  ops: RunItemOp[];
}

/** The run view's whole state: the ordered items plus the derived counters
 *  the header renders, so each streamed operation folds in O(1) instead of
 *  rescanning the list. */
export interface RunState {
  items: RunItem[];
  /** Suggestion id -> index into `items`; built once, never mutated. */
  index: Map<string, number>;
  /** Index of the first suggestion without a final result; the one the
   *  service is executing or will execute next. -1 when all are final. */
  activeIndex: number;
  /** Suggestions that have reached a final result. */
  done: number;
}

export function isFinal(status: RunItemStatus): status is EntryResult {
  return (
    status === 'executed' ||
    status === 'partial' ||
    status === 'failed' ||
    status === 'skipped'
  );
}

/**
 * Build the run state for the requested ids, in the service's execution
 * order: moves first, then deletes, each ordered by source path (see
 * "Transactional Safety" in DESIGN.md; the service case-folds that key on
 * Windows, which a display-order approximation here skips). Suggestions
 * already loaded in the browser window label themselves from their own
 * record; the rest fall back to their id until `fillRunLabels` fetches them.
 */
export function buildRunState(
  ids: string[],
  known: Map<string, Suggestion>,
  root: string | null,
): RunState {
  const records = ids
    .map((id) => ({ id, s: known.get(id) ?? null }))
    .map((r) => ({
      ...r,
      type: r.s?.type ?? 'delete',
      target: r.s?.target ?? r.id,
    }));
  // Mirror the service's deterministic execution order so the live list
  // reads top-down as the run proceeds.
  records.sort((a, b) => {
    const movesFirst = Number(a.type !== 'move') - Number(b.type !== 'move');
    if (movesFirst !== 0) return movesFirst;
    return a.target < b.target ? -1 : a.target > b.target ? 1 : 0;
  });
  const items: RunItem[] = records.map((r) => ({
    id: r.id,
    // The execution list reports what happened, so its verbs are past tense;
    // the browse rows keep the imperative form for proposals.
    parts: r.s
      ? actionLabelParts(r.s, root, { past: true })
      : [{ kind: 'text', text: r.id }],
    status: 'pending',
    ops: [],
  }));
  const index = new Map(items.map((it, i) => [it.id, i] as const));
  return { items, index, activeIndex: items.length > 0 ? 0 : -1, done: 0 };
}

/**
 * Result a finished suggestion's own operations derive, mirroring the
 * service's report rules (crates/execute `execute_one`): no operations is a
 * skip (a cancelled run marks the remaining suggestions this way), a failed
 * parent operation fails the unit, and a completed parent is partial when a
 * dependent repair failed or the parent itself reported only partial
 * completion (a published cross-filesystem copy whose source removal failed,
 * carried as a non-null detail on the completed move).
 */
export function deriveResult(ops: RunItemOp[]): EntryResult {
  if (ops.length === 0) return 'skipped';
  // The parent operation is the move or delete; if the same operation ever
  // streams more than one event (the documented status enum includes a
  // `started` state the current service does not emit), the latest event for
  // it wins.
  const parents = ops.filter((o) => o.operation !== 'repair_symlink');
  const parent = parents.length > 0 ? parents[parents.length - 1] : ops[ops.length - 1];
  if (parent.status === 'failed') return 'failed';
  const repairFailed = ops.some(
    (o) => o.operation === 'repair_symlink' && o.status === 'failed',
  );
  return repairFailed || parent.status !== 'completed' || !!parent.detail
    ? 'partial'
    : 'executed';
}

/**
 * Fold one `execution.op` event into the run state. The service executes
 * one suggestion at a time and publishes its operations only once the whole
 * suggestion has finished, so when operations arrive for a suggestion past
 * the active one, every earlier unfinished suggestion is final and derives
 * its result from the operations it accumulated; the report later confirms
 * or corrects these derived results.
 */
export function applyExecutionOp(prev: RunState, op: ExecutionOp): RunState {
  const idx = prev.index.get(op.suggestion_id);
  if (idx === undefined) return prev;
  const item = prev.items[idx];
  if (isFinal(item.status)) return prev;
  const items = prev.items.slice();
  let done = prev.done;
  let activeIndex = prev.activeIndex;
  if (idx > activeIndex) {
    // The run moved past the active suggestion: finalize it (and anything
    // else still open before this one) from its own operations.
    for (let i = activeIndex; i < idx; i++) {
      if (!isFinal(items[i].status)) {
        items[i] = { ...items[i], status: deriveResult(items[i].ops) };
        done += 1;
      }
    }
    activeIndex = idx;
  } else if (idx < activeIndex) {
    // A late operation for an already-passed suggestion: apply it, but the
    // active pointer stays ahead.
  }
  items[idx] = {
    ...item,
    status: 'running',
    ops: [
      ...item.ops,
      {
        operation: op.operation,
        status: op.status,
        detail: op.detail?.detail ?? null,
      },
    ],
  };
  return { items, index: prev.index, activeIndex, done };
}

/**
 * Merge the authoritative execution report into the run state: every entry
 * fixes its suggestion's result, and a requested id without an entry was
 * not executed (already executed, rejected at validation, or skipped by a
 * cancellation with no operations), which reads as skipped.
 */
export function applyReport(prev: RunState, entries: ReportEntry[]): RunState {
  const items = prev.items.slice();
  let done = prev.done;
  for (const e of entries) {
    const idx = prev.index.get(e.suggestion_id);
    if (idx === undefined) continue;
    if (items[idx].status !== e.result) {
      if (!isFinal(items[idx].status)) done += 1;
      items[idx] = { ...items[idx], status: e.result };
    }
  }
  for (let i = 0; i < items.length; i++) {
    if (!isFinal(items[i].status)) {
      items[i] = { ...items[i], status: 'skipped', ops: [] };
      done += 1;
    }
  }
  return { items, index: prev.index, activeIndex: -1, done };
}
