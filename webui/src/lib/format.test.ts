import { describe, expect, it } from 'vitest';
import {
  ALL_FILTER,
  STATUS_CYCLE,
  TYPE_CYCLE,
  cycleNext,
  matchesFilter,
  uniqueKinds,
} from './filter';
import {
  asciiOnly,
  countsOf,
  countsSummary,
  entryLine,
  rowLabel,
  shorten,
  tail,
} from './format';
import type { ReportEntry, Suggestion } from '../types';

function sampleSuggestion(over: Partial<Suggestion> = {}): Suggestion {
  const base: Suggestion = {
    id: 's-abc',
    type: 'move',
    asset_kind: 'photo',
    target: '/home/a/IMG_1.jpg',
    new_path: '/home/a/Pictures/2024/IMG_1.jpg',
    reason: 'organize',
    selected: false,
    status: 'pending',
    dependent_operations: [
      { type: 'repair_symlink', link_path: '/home/a/L', replacement_target: '/home/a/Pictures/2024' },
    ],
  };
  return { ...base, ...over };
}

describe('filter', () => {
  it('matches on all three dimensions', () => {
    const s = sampleSuggestion({ asset_kind: 'photo', status: 'pending' });
    expect(matchesFilter(s, ALL_FILTER)).toBe(true);
    expect(matchesFilter(s, { type: 'all', kind: 'photo', status: 'all' })).toBe(true);
    expect(matchesFilter(s, { type: 'delete', kind: 'all', status: 'all' })).toBe(false);
    expect(matchesFilter(s, { type: 'all', kind: 'audio', status: 'all' })).toBe(false);
    expect(matchesFilter(s, { type: 'all', kind: 'all', status: 'executed' })).toBe(false);
  });

  it('cycles values with wraparound', () => {
    expect(cycleNext(TYPE_CYCLE, 'all')).toBe('move');
    expect(cycleNext(TYPE_CYCLE, 'delete')).toBe('all');
    expect(cycleNext(STATUS_CYCLE, 'executed')).toBe('failed');
    expect(cycleNext(STATUS_CYCLE, 'skipped')).toBe('all');
  });

  it('collects unique sorted kinds', () => {
    const items = [
      sampleSuggestion({ asset_kind: 'photo' }),
      sampleSuggestion({ asset_kind: 'audio' }),
      sampleSuggestion({ asset_kind: 'photo' }),
      sampleSuggestion({ asset_kind: '' }),
    ];
    expect(uniqueKinds(items)).toEqual(['audio', 'photo']);
  });
});

describe('format', () => {
  it('tails keep the last segments', () => {
    expect(tail('/a/b/c/d.jpg', 2)).toBe('/c/d.jpg');
    expect(tail('/a/b.jpg', 3)).toBe('/a/b.jpg');
    expect(tail('/', 3)).toBe('/');
    expect(tail('', 1)).toBe('/');
  });



  it('shorten elides the middle', () => {
    const long = 'x'.repeat(80);
    expect(shorten(long, 40).length).toBe(40);
    expect(shorten(long, 40).startsWith('...')).toBe(true);
    expect(shorten('abc',10)).toBe('abc');
  });


  it('row label is ascii, marks selection, and reads move/delete', () => {
    const label = rowLabel(sampleSuggestion());
    expect(label.startsWith('[ ]')).toBe(true);
    expect(label).toContain('move /home/a/IMG_1.jpg to /Pictures/2024/IMG_1.jpg');
    const sel = rowLabel(sampleSuggestion({ selected: true }));
    expect(sel.startsWith('[x]')).toBe(true);
    const del = rowLabel(sampleSuggestion({
      type: 'delete',
      new_path: null,
      metadata: { duplicate_of: '/home/a/keeper/IMG_1.jpg' },
    }));
    expect(del).toContain('delete /home/a/IMG_1.jpg, duplicate of /a/keeper/IMG_1.jpg');
    const delNoMeta = rowLabel(sampleSuggestion({ type: 'delete', new_path: null }));
    expect(delNoMeta).toContain('delete /home/a/IMG_1.jpg');
    expect(delNoMeta).not.toContain('duplicate of');
    expect(asciiOnly(label)).toBe(true);
  });

  it('row label marks pending provider metadata', () => {
    const pending = rowLabel(sampleSuggestion({ metadata_pending: true }));
    expect(pending).toContain('[metadata pending]');
    const resolved = rowLabel(sampleSuggestion({ metadata_pending: false }));
    expect(resolved).not.toContain('[metadata pending]');
  });


  it('counts and entry lines render reports', () => {
    const entries: ReportEntry[] = [
      { suggestion_id: 's-1', result: 'executed', operations: [] },
      { suggestion_id: 's-2', result: 'failed', operations: [] },
      { suggestion_id: 's-3', result: 'skipped', operations: [] },
    ];
    expect(countsOf(entries)).toEqual({ executed: 1, failed: 1, skipped: 1 });
    expect(countsSummary(countsOf(entries)).includes('executed=1')).toBe(true);
    expect(countsSummary({ executed: 0, failed: 0, skipped: 0 })).toBe('no entries executed');
    expect(entryLine({ suggestion_id: 's-9', result: 'executed', operations: [] })).toBe('[executed] s-9');
  });
});