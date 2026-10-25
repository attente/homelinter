import { describe, expect, it } from 'vitest';
import { applySelectionResult } from './selection';
import type { Suggestion } from '../types';

function row(id: string, selected: boolean): Suggestion {
  return { id, selected, status: 'pending' } as Suggestion;
}

describe('selection', () => {
  it('clears an optimistic row the service did not select', () => {
    const stale = row('stale', true);
    const kept = row('kept', true);
    const next = applySelectionResult(
      [stale, kept],
      { ids: ['kept'], rejected: [] },
    );
    expect(next.map((s) => [s.id, s.selected])).toEqual([
      ['stale', false],
      ['kept', true],
    ]);
  });

  it('selects rows the service added and keeps unchanged rows by identity', () => {
    const added = row('added', false);
    const untouched = row('other', false);
    const next = applySelectionResult(
      [added, untouched],
      { ids: ['added'], rejected: [] },
    );
    expect(next[0]).not.toBe(added);
    expect(next[0].selected).toBe(true);
    expect(next[1]).toBe(untouched);
  });

  it('marks rejected rows with the service reason and clears them', () => {
    const stale = row('stale', true);
    const untouched = row('other', false);
    const next = applySelectionResult(
      [stale, untouched],
      {
        ids: [],
        rejected: [{ id: 'stale', reason: 'destination_collision:also=/b' }],
      },
    );
    expect(next[0]).not.toBe(stale);
    expect(next[0].selected).toBe(false);
    expect(next[0].status).toBe('rejected');
    expect(next[0].rejection_reason).toBe('destination_collision:also=/b');
    expect(next[1]).toBe(untouched);
  });
});
