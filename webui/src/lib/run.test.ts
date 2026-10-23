import { describe, expect, it } from 'vitest';
import { actionText } from './format';
import {
  applyExecutionOp,
  applyReport,
  buildRunState,
  deriveResult,
  type RunState,
} from './run';
import type { ExecutionOp, ReportEntry, Suggestion } from '../types';

function suggestion(over: Partial<Suggestion> = {}): Suggestion {
  return {
    id: 's-1',
    type: 'move',
    asset_kind: 'photo',
    target: '/r/IMG_1.jpg',
    new_path: '/r/Pictures/IMG_1.jpg',
    reason: 'organize',
    selected: true,
    status: 'pending',
    ...over,
  };
}

function op(over: Partial<ExecutionOp> = {}): ExecutionOp {
  return {
    suggestion_id: 's-1',
    operation: 'move',
    status: 'completed',
    detail: { source: '/r/IMG_1.jpg', destination: '/r/Pictures/IMG_1.jpg', detail: null },
    ...over,
  };
}

function stateWith(...items: { id: string; type: 'move' | 'delete'; target: string }[]): RunState {
  const known = new Map(
    items.map((it) => [it.id, suggestion({ id: it.id, type: it.type, target: it.target })] as const),
  );
  return buildRunState(items.map((it) => it.id), known, '/r');
}

describe('buildRunState', () => {
  it('orders moves before deletes, each by source path', () => {
    const run = stateWith(
      { id: 'd-2', type: 'delete', target: '/r/a.jpg' },
      { id: 'm-2', type: 'move', target: '/r/b.jpg' },
      { id: 'd-1', type: 'delete', target: '/r/0.jpg' },
      { id: 'm-1', type: 'move', target: '/r/a.jpg' },
    );
    expect(run.items.map((i) => i.id)).toEqual(['m-1', 'm-2', 'd-1', 'd-2']);
    expect(run.items.every((i) => i.status === 'pending')).toBe(true);
    expect(run.activeIndex).toBe(0);
    expect(run.done).toBe(0);
  });

  it('labels rows with the action sentence, falling back to the id', () => {
    const known = new Map([
      ['m-1', suggestion({ id: 'm-1', target: '/r/a.jpg' })] as const,
    ]);
    const run = buildRunState(['m-1', 'x-9'], known, '/r');
    expect(actionText(run.items[0].parts)).toBe(
      'move a.jpg to Pictures/IMG_1.jpg',
    );
    expect(run.items[0].parts.map((p) => p.kind)).toEqual([
      'text',
      'path',
      'text',
      'path',
    ]);
    expect(actionText(run.items[1].parts)).toBe('x-9');
    expect(run.items[1].parts).toEqual([{ kind: 'text', text: 'x-9' }]);
  });
});

describe('applyExecutionOp', () => {
  it('marks the target running and derives nothing yet', () => {
    const run = applyExecutionOp(
      stateWith({ id: 'm-1', type: 'move', target: '/r/a.jpg' }),
      op({ suggestion_id: 'm-1' }),
    );
    expect(run.items[0].status).toBe('running');
    expect(run.done).toBe(0);
    expect(run.items[0].ops).toHaveLength(1);
  });

  it('derives executed for a completed move once the run moves on', () => {
    let run = stateWith(
      { id: 'm-1', type: 'move', target: '/r/a.jpg' },
      { id: 'm-2', type: 'move', target: '/r/b.jpg' },
    );
    run = applyExecutionOp(run, op({ suggestion_id: 'm-1' }));
    run = applyExecutionOp(run, op({ suggestion_id: 'm-2' }));
    expect(run.items[0].status).toBe('executed');
    expect(run.items[1].status).toBe('running');
    expect(run.done).toBe(1);
    expect(run.activeIndex).toBe(1);
  });

  it('derives partial when a dependent repair failed', () => {
    let run = stateWith(
      { id: 'm-1', type: 'move', target: '/r/a.jpg' },
      { id: 'm-2', type: 'move', target: '/r/b.jpg' },
    );
    run = applyExecutionOp(run, op({ suggestion_id: 'm-1' }));
    run = applyExecutionOp(
      run,
      op({ suggestion_id: 'm-1', operation: 'repair_symlink', status: 'failed' }),
    );
    run = applyExecutionOp(run, op({ suggestion_id: 'm-2' }));
    expect(run.items[0].status).toBe('partial');
  });

  it('derives failed from a failed parent operation', () => {
    let run = stateWith(
      { id: 'd-1', type: 'delete', target: '/r/a.jpg' },
      { id: 'd-2', type: 'delete', target: '/r/b.jpg' },
    );
    run = applyExecutionOp(
      run,
      op({ suggestion_id: 'd-1', operation: 'delete', status: 'failed' }),
    );
    run = applyExecutionOp(run, op({ suggestion_id: 'd-2', operation: 'delete' }));
    expect(run.items[0].status).toBe('failed');
  });

  it('ignores operations for suggestions outside the run or already final', () => {
    let run = stateWith({ id: 'm-1', type: 'move', target: '/r/a.jpg' });
    expect(applyExecutionOp(run, op({ suggestion_id: 'nope' }))).toBe(run);
    run = applyExecutionOp(run, op({ suggestion_id: 'm-1' }));
    run = applyReport(run, [{ suggestion_id: 'm-1', result: 'executed', operations: [] }]);
    expect(applyExecutionOp(run, op({ suggestion_id: 'm-1' }))).toBe(run);
  });
});

describe('deriveResult', () => {
  it('reads the same rules the service reports by', () => {
    expect(deriveResult([])).toBe('skipped');
    expect(deriveResult([{ operation: 'move', status: 'failed' }])).toBe('failed');
    expect(deriveResult([{ operation: 'move', status: 'completed' }])).toBe('executed');
    expect(
      deriveResult([
        { operation: 'move', status: 'completed' },
        { operation: 'repair_symlink', status: 'failed' },
      ]),
    ).toBe('partial');
    // A completed move that could not finish its source cleanup reports a
    // detail on the parent operation: partial, not executed.
    expect(
      deriveResult([{ operation: 'move', status: 'completed', detail: 'source cleanup failed' }]),
    ).toBe('partial');
    // A later event for the same operation (the documented status enum
    // includes a `started` state) replaces the earlier one.
    expect(
      deriveResult([
        { operation: 'move', status: 'started' },
        { operation: 'move', status: 'completed' },
      ]),
    ).toBe('executed');
    // Skipped repairs without a failure do not downgrade the result.
    expect(
      deriveResult([
        { operation: 'move', status: 'completed' },
        { operation: 'repair_symlink', status: 'skipped' },
      ]),
    ).toBe('executed');
  });
});

describe('applyReport', () => {
  it('fixes every result and marks unreported ids skipped', () => {
    let run = stateWith(
      { id: 'm-1', type: 'move', target: '/r/a.jpg' },
      { id: 'm-2', type: 'move', target: '/r/b.jpg' },
      { id: 'd-1', type: 'delete', target: '/r/c.jpg' },
    );
    run = applyExecutionOp(run, op({ suggestion_id: 'm-1' }));
    run = applyExecutionOp(run, op({ suggestion_id: 'm-2' }));
    run = applyReport(run, [
      { suggestion_id: 'm-1', result: 'executed', operations: [] },
      { suggestion_id: 'm-2', result: 'partial', operations: [] },
    ] satisfies ReportEntry[]);
    expect(run.items.map((i) => i.status)).toEqual(['executed', 'partial', 'skipped']);
    expect(run.done).toBe(3);
    expect(run.activeIndex).toBe(-1);
  });
});
