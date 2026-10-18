import { describe, expect, it } from 'vitest';
import {
  ALL_FILTER,
  MANUAL_CYCLE,
  STATUS_CYCLE,
  TYPE_CYCLE,
  cycleNext,
  matchesFilter,
  mergeSuggestionEvents,
  uniqueKinds,
} from './filter';
import {
  asciiOnly,
  countsOf,
  countsSummary,
  entryLine,
  manualOverrideLabel,
  pathLabel,
  rowLabel,
  shorten,
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
  it('matches on all four dimensions', () => {
    const s = sampleSuggestion({ asset_kind: 'photo', status: 'pending' });
    expect(matchesFilter(s, ALL_FILTER)).toBe(true);
    expect(
      matchesFilter(s, { type: 'all', kind: 'photo', status: 'all', manual: 'all' }),
    ).toBe(true);
    expect(
      matchesFilter(s, { type: 'delete', kind: 'all', status: 'all', manual: 'all' }),
    ).toBe(false);
    expect(
      matchesFilter(s, { type: 'all', kind: 'audio', status: 'all', manual: 'all' }),
    ).toBe(false);
    expect(
      matchesFilter(s, { type: 'all', kind: 'all', status: 'executed', manual: 'all' }),
    ).toBe(false);
  });

  it('manual filter keeps only suggestions with saved overrides', () => {
    const manualFilter = { ...ALL_FILTER, manual: 'manual' as const };
    expect(matchesFilter(sampleSuggestion(), manualFilter)).toBe(false);
    expect(
      matchesFilter(sampleSuggestion({ manual_overrides: [] }), manualFilter),
    ).toBe(false);
    expect(
      matchesFilter(
        sampleSuggestion({ manual_overrides: ['location'] }),
        manualFilter,
      ),
    ).toBe(true);
    expect(matchesFilter(sampleSuggestion(), ALL_FILTER)).toBe(true);
  });

  it('cycles values with wraparound', () => {
    expect(cycleNext(TYPE_CYCLE, 'all')).toBe('move');
    expect(cycleNext(TYPE_CYCLE, 'delete')).toBe('all');
    expect(cycleNext(STATUS_CYCLE, 'executed')).toBe('partial');
    expect(cycleNext(STATUS_CYCLE, 'partial')).toBe('failed');
    expect(cycleNext(STATUS_CYCLE, 'skipped')).toBe('all');
    expect(cycleNext(MANUAL_CYCLE, 'all')).toBe('manual');
    expect(cycleNext(MANUAL_CYCLE, 'manual')).toBe('all');
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

  it('streamed updates respect the active filter', () => {
    const filter = {
      type: 'all' as const,
      kind: 'all' as const,
      status: 'pending' as const,
      manual: 'all' as const,
    };
    const loaded = sampleSuggestion({ id: 's-1', status: 'pending' });

    // A new row that does not match the filter is not appended.
    const leaked = mergeSuggestionEvents(
      [loaded],
      new Map([['s-2', sampleSuggestion({ id: 's-2', status: 'rejected' })]]),
      new Set(),
      filter,
    );
    expect(leaked.changed).toBe(false);
    expect(leaked.suggestions.map((s) => s.id)).toEqual(['s-1']);

    // A new row that matches is appended.
    const appended = mergeSuggestionEvents(
      [loaded],
      new Map([['s-3', sampleSuggestion({ id: 's-3', status: 'pending' })]]),
      new Set(),
      filter,
    );
    expect(appended.changed).toBe(true);
    expect(appended.suggestions.map((s) => s.id)).toEqual(['s-1', 's-3']);

    // An update that pushes a loaded row out of the filter drops it.
    const dropped = mergeSuggestionEvents(
      [loaded],
      new Map([['s-1', sampleSuggestion({ id: 's-1', status: 'rejected' })]]),
      new Set(),
      filter,
    );
    expect(dropped.changed).toBe(true);
    expect(dropped.suggestions).toEqual([]);

    // Removals still apply.
    const removed = mergeSuggestionEvents([loaded], new Map(), new Set(['s-1']), ALL_FILTER);
    expect(removed.changed).toBe(true);
    expect(removed.suggestions).toEqual([]);
  });
});

describe('format', () => {
  it('path labels are scan-root relative, verbatim to three segments', () => {
    const root = '/home/a';
    expect(pathLabel('/home/a/IMG_1.jpg', root)).toBe('IMG_1.jpg');
    expect(pathLabel('/home/a/raw/IMG_1.jpg', root)).toBe('raw/IMG_1.jpg');
    expect(pathLabel('/home/a/Pictures/2024/IMG_1.jpg', root)).toBe('Pictures/2024/IMG_1.jpg');
    expect(pathLabel('/home/a/Pictures/2024/03/IMG_1.jpg', root)).toBe('.../2024/03/IMG_1.jpg');
    expect(pathLabel('/home/a', root)).toBe('/');
    expect(pathLabel('/other/x.jpg', root)).toBe('/other/x.jpg');
    expect(pathLabel('/home/a/1/2/3/4.jpg', root)).toBe('.../2/3/4.jpg');
    expect(pathLabel('/home/a/Pictures/2024/IMG_1.jpg')).toBe('.../Pictures/2024/IMG_1.jpg');
  });



  it('shorten elides the middle', () => {
    const long = 'x'.repeat(80);
    expect(shorten(long, 40).length).toBe(40);
    expect(shorten(long, 40).startsWith('...')).toBe(true);
    expect(shorten('abc',10)).toBe('abc');
  });


  it('row label is ascii, marks selection, and reads move/delete', () => {
    const root = '/home/a';
    const label = rowLabel(sampleSuggestion(), root);
    expect(label.startsWith('[ ]')).toBe(true);
    expect(label).toContain('move IMG_1.jpg to Pictures/2024/IMG_1.jpg');
    const sel = rowLabel(sampleSuggestion({ selected: true }), root);
    expect(sel.startsWith('[x]')).toBe(true);
    const del = rowLabel(sampleSuggestion({
      type: 'delete',
      new_path: null,
      metadata: { duplicate_of: '/home/a/keeper/IMG_1.jpg' },
    }), root);
    expect(del).toContain('delete IMG_1.jpg, duplicate of keeper/IMG_1.jpg');
    const delNoMeta = rowLabel(sampleSuggestion({ type: 'delete', new_path: null }), root);
    expect(delNoMeta).toContain('delete IMG_1.jpg');
    expect(delNoMeta).not.toContain('duplicate of');
    expect(asciiOnly(label)).toBe(true);
  });

  it('manual override labels join fields like the service diagnostic', () => {
    expect(manualOverrideLabel(undefined)).toBe('');
    expect(manualOverrideLabel([])).toBe('');
    expect(manualOverrideLabel(['location'])).toBe('location');
    expect(manualOverrideLabel(['location', 'subject'])).toBe('location and subject');
    expect(manualOverrideLabel(['location', 'subject', 'author'])).toBe(
      'location, subject, and author',
    );
  });

  it('row label shows a scanning placeholder while provider metadata is pending', () => {
    const pending = rowLabel(sampleSuggestion({ metadata_pending: true }));
    expect(pending).toBe('[ ] scanning IMG_1.jpg');
    const resolved = rowLabel(sampleSuggestion({ metadata_pending: false }), '/home/a');
    expect(resolved).toContain('move IMG_1.jpg to');
    expect(resolved).not.toContain('scanning');
  });

  it('row label shows the rejection reason when present', () => {
    const s = sampleSuggestion({
      status: 'rejected',
      rejection_reason: 'destination_collision:also=abc123',
    });
    const label = rowLabel(s);
    expect(label).toContain('(rejected)');
    expect(label).toContain('[rejected: destination_collision:also=abc123]');
    const plain = rowLabel(sampleSuggestion({ status: 'rejected' }));
    expect(plain).not.toContain('[rejected:');
  });


  it('counts and entry lines render reports', () => {
    const entries: ReportEntry[] = [
      { suggestion_id: 's-1', result: 'executed', operations: [] },
      { suggestion_id: 's-2', result: 'failed', operations: [] },
      { suggestion_id: 's-3', result: 'skipped', operations: [] },
      { suggestion_id: 's-4', result: 'partial', operations: [] },
    ];
    expect(countsOf(entries)).toEqual({
      executed: 1,
      partial: 1,
      failed: 1,
      skipped: 1,
    });
    expect(countsSummary(countsOf(entries)).includes('partial=1')).toBe(true);
    expect(countsSummary({ executed: 0, partial: 0, failed: 0, skipped: 0 })).toBe(
      'no entries executed',
    );
    expect(entryLine({ suggestion_id: 's-9', result: 'executed', operations: [] })).toBe('[executed] s-9');
  });
});