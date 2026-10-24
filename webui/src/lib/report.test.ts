import { describe, expect, it } from 'vitest';
import { asciiOnly } from './format';
import { applyReport, buildRunState, type RunState } from './run';
import {
  executionReportFilename,
  executionReportText,
  reportActions,
} from './report';
import type {
  ExecutionReport,
  ReportEntry,
  Suggestion,
} from '../types';

function suggestion(over: Partial<Suggestion>): Suggestion {
  return {
    id: over.id ?? 's-1',
    type: over.type ?? 'move',
    asset_kind: 'photo',
    target: over.target ?? '/scan/a.jpg',
    new_path: over.new_path ?? '/scan/keep/a.jpg',
    reason: 'organize',
    selected: true,
    status: 'pending',
    ...over,
  };
}

const AT = new Date('2025-03-05T14:02:11.000Z');

const entries: ReportEntry[] = [
  {
    suggestion_id: 's-1',
    result: 'executed',
    operations: [
      {
        operation: 'move',
        source: '/scan/a.jpg',
        destination: '/scan/keep/a.jpg',
        status: 'completed',
      },
    ],
  },
  {
    suggestion_id: 's-2',
    result: 'failed',
    operations: [
      {
        operation: 'move',
        source: '/scan/b.jpg',
        destination: '/scan/keep/b.jpg',
        status: 'failed',
        detail: 'destination exists; refusing to overwrite',
      },
    ],
  },
  {
    suggestion_id: 's-3',
    result: 'partial',
    operations: [
      {
        operation: 'move',
        source: '/scan/old/c.jpg',
        destination: '/scan/keep/c.jpg',
        status: 'completed',
        detail: 'source removal failed; leftover at /scan/old/c.jpg',
      },
      {
        operation: 'repair_symlink',
        source: '/scan/old/c.jpg',
        destination: '/scan/keep/c.jpg',
        status: 'failed',
        detail: 'link is unreadable',
      },
    ],
  },
];

function runState(): RunState {
  const known = new Map(
    [
      suggestion({ id: 's-1', target: '/scan/a.jpg', new_path: '/scan/keep/a.jpg' }),
      suggestion({ id: 's-2', target: '/scan/b.jpg', new_path: '/scan/keep/b.jpg' }),
      suggestion({ id: 's-3', target: '/scan/old/c.jpg', new_path: '/scan/keep/c.jpg' }),
      suggestion({
        id: 's-5',
        type: 'delete',
        target: '/scan/dup.jpg',
        metadata: { duplicate_of: '/scan/keeper.jpg' },
      }),
    ].map((s) => [s.id, s] as const),
  );
  return applyReport(
    buildRunState(['s-1', 's-2', 's-3', 's-5'], known, '/scan'),
    entries,
  );
}

function report(): ExecutionReport {
  return {
    entries,
    partial: true,
    scan_root: '/scan',
    platform: 'linux',
  };
}

describe('executionReportText', () => {
  it('orders the summary, the errors, and then every action', () => {
    const text = executionReportText({
      report: report(),
      run: runState(),
      generatedAt: AT,
    });
    const summaryAt = text.indexOf('summary (4 actions)');
    const errorsAt = text.indexOf('errors (2)');
    const actionsAt = text.indexOf('actions (4)');
    expect(summaryAt).toBeGreaterThan(-1);
    expect(errorsAt).toBeGreaterThan(summaryAt);
    expect(actionsAt).toBeGreaterThan(errorsAt);
    expect(text.indexOf('run error:')).toBe(-1);
  });

  it('counts results, including requested suggestions the report omits', () => {
    const text = executionReportText({
      report: report(),
      run: runState(),
      generatedAt: AT,
    });
    expect(text).toContain('  executed 1');
    expect(text).toContain('  partial  1');
    expect(text).toContain('  failed   1');
    expect(text).toContain('  skipped  1');
    expect(text).toContain('[skipped] delete dup.jpg, duplicate of keeper.jpg');
  });

  it('records failed operations and their details in the errors section', () => {
    const text = executionReportText({
      report: report(),
      run: runState(),
      generatedAt: AT,
    });
    expect(text).toContain(
      '[failed] move /scan/b.jpg to /scan/keep/b.jpg',
    );
    expect(text).toContain(
      '    move: failed /scan/b.jpg -> /scan/keep/b.jpg - destination exists; refusing to overwrite',
    );
    expect(text).toContain(
      '    repair_symlink: failed /scan/old/c.jpg -> /scan/keep/c.jpg - link is unreadable',
    );
  });

  it('lists every action with its notable operation outcomes', () => {
    const text = executionReportText({
      report: report(),
      run: runState(),
      generatedAt: AT,
    });
    expect(text).toContain('[executed] move /scan/a.jpg to /scan/keep/a.jpg');
    // A plain completed parent operation repeats the action line, so it is
    // left out; a completed parent carrying a detail (the partial
    // cross-filesystem move) is kept.
    expect(text).not.toContain(
      '    move: completed /scan/a.jpg -> /scan/keep/a.jpg',
    );
    expect(text).toContain(
      '    move: completed /scan/old/c.jpg -> /scan/keep/c.jpg - source removal failed; leftover at /scan/old/c.jpg',
    );
    expect(text).toContain('    no operations recorded');
  });

  it('reports a run-level error and the run metadata before the errors', () => {
    const text = executionReportText({
      report: report(),
      run: runState(),
      error: 'scan was cancelled',
      generatedAt: AT,
    });
    expect(text).toContain('run error: scan was cancelled');
    expect(text).toContain('scan root: /scan');
    expect(text).toContain('platform: linux');
    expect(text).toContain('generated: 2025-03-05T14:02:11.000Z');
    expect(text).toContain('partial: yes');
    expect(text.indexOf('run error:')).toBeLessThan(text.indexOf('errors (2)'));
  });

  it('falls back to report operations when no run model is available', () => {
    const actions = reportActions(report(), null);
    expect(actions.map((a) => a.label)).toEqual([
      'move /scan/a.jpg to /scan/keep/a.jpg',
      'move /scan/b.jpg to /scan/keep/b.jpg',
      'move /scan/old/c.jpg to /scan/keep/c.jpg',
    ]);
    expect(actions.map((a) => a.status)).toEqual([
      'executed',
      'failed',
      'partial',
    ]);
  });

  it('stays ASCII only', () => {
    const text = executionReportText({
      report: report(),
      run: runState(),
      generatedAt: AT,
    });
    expect(asciiOnly(text)).toBe(true);
    expect(text.endsWith('\n')).toBe(true);
  });
});

describe('executionReportFilename', () => {
  it('stamps a filesystem-safe timestamp', () => {
    expect(executionReportFilename(AT)).toBe(
      'homelinter-execution-report-2025-03-05-14-02-11.txt',
    );
  });
});
