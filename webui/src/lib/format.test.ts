import { describe, expect, it } from 'vitest';
import {
  ALL_FILTER,
  MANUAL_OPTIONS,
  STATUS_OPTIONS,
  TYPE_OPTIONS,
  isDefaultFilter,
  matchesFilter,
  mergeSuggestionEvents,
  uniqueKinds,
  filterFromSearchParams,
  filterToSearchParams,
} from './filter';
import {
  asciiOnly,
  cacheStatsLabel,
  countsOf,
  countsSummary,
  entryLine,
  manualOverrideLabel,
  metadataProgressLabel,
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

  it('offers one combo-box option per value, with an unconstrained first entry', () => {
    expect(TYPE_OPTIONS.map((o) => o.value)).toEqual(['all', 'move', 'delete']);
    expect(STATUS_OPTIONS.map((o) => o.value)).toEqual([
      'all',
      'pending',
      'rejected',
      'executed',
      'partial',
      'failed',
      'skipped',
    ]);
    expect(MANUAL_OPTIONS.map((o) => o.value)).toEqual(['all', 'manual']);
    for (const options of [TYPE_OPTIONS, STATUS_OPTIONS, MANUAL_OPTIONS]) {
      expect(options[0].value).toBe('all');
      const labels = options.map((o) => o.label);
      expect(new Set(labels).size).toBe(labels.length);
    }
  });

  it('recognizes the unconstrained filter', () => {
    expect(isDefaultFilter(ALL_FILTER)).toBe(true);
    expect(isDefaultFilter({ ...ALL_FILTER, kind: 'photo' })).toBe(false);
    expect(isDefaultFilter({ ...ALL_FILTER, status: 'failed' })).toBe(false);
    expect(isDefaultFilter({ ...ALL_FILTER, manual: 'manual' })).toBe(false);
    expect(isDefaultFilter({ ...ALL_FILTER, type: 'move' })).toBe(false);
    expect(isDefaultFilter({ ...ALL_FILTER, path: '' })).toBe(true);
    expect(isDefaultFilter({ ...ALL_FILTER, path: 'IMG' })).toBe(false);
  });

  it('matches the path filter against source, destination, and keeper', () => {
    const s = sampleSuggestion();
    // An empty (or absent) path filter is unconstrained.
    expect(matchesFilter(s, { ...ALL_FILTER, path: '' })).toBe(true);
    expect(matchesFilter(s, ALL_FILTER)).toBe(true);
    // The source target ("from") and the move destination ("to") both match.
    expect(matchesFilter(s, { ...ALL_FILTER, path: 'IMG_1' })).toBe(true);
    expect(matchesFilter(s, { ...ALL_FILTER, path: 'pictures/2024' })).toBe(true);
    // Matching is case-insensitive and substring-based.
    expect(matchesFilter(s, { ...ALL_FILTER, path: 'img_1.jpg' })).toBe(true);
    expect(matchesFilter(s, { ...ALL_FILTER, path: '/home/a' })).toBe(true);
    expect(matchesFilter(s, { ...ALL_FILTER, path: 'VIDEOS' })).toBe(false);
    // The retained keeper of a duplicate delete ("original") matches too.
    const dup = sampleSuggestion({
      type: 'delete',
      target: '/home/a/copy.jpg',
      new_path: undefined,
      metadata: { duplicate_of: '/home/a/Original.jpg' },
    });
    expect(matchesFilter(dup, { ...ALL_FILTER, path: 'original' })).toBe(true);
    expect(matchesFilter(dup, { ...ALL_FILTER, path: 'copy.jpg' })).toBe(true);
    expect(matchesFilter(dup, { ...ALL_FILTER, path: 'pictures' })).toBe(false);
  });

  it('round-trips the filter through query parameters', () => {
    const q = filterToSearchParams({ ...ALL_FILTER, type: 'move', path: '/photos/' });
    expect(q.toString()).toBe('type=move&path=%2Fphotos%2F');
    const restored = filterFromSearchParams(new URLSearchParams(q.toString()));
    expect(restored.type).toBe('move');
    expect(restored.path).toBe('/photos/');
    expect(restored.kind).toBe('all');
    expect(restored.status).toBe('all');
    expect(restored.manual).toBe('all');
    // Unrecognized values fall back to the defaults; an empty URL yields the
    // fully unconstrained filter.
    expect(filterFromSearchParams(new URLSearchParams(''))).toEqual(ALL_FILTER);
    const junk = filterFromSearchParams(
      new URLSearchParams('type=copy&kind=&status=nope&manual=true&path='),
    );
    expect(junk.type).toBe('all');
    expect(junk.kind).toBe('all');
    expect(junk.status).toBe('all');
    expect(junk.manual).toBe('all');
    expect(junk.path).toBe('');
    // A fully cleared filter serializes to no query string at all.
    expect(filterToSearchParams(ALL_FILTER).toString()).toBe('');
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

  it('cache counters read as two named counts, singular when one', () => {
    expect(cacheStatsLabel(0, 0)).toBe('0 hits, 0 misses');
    expect(cacheStatsLabel(12, 3)).toBe('12 hits, 3 misses');
    expect(cacheStatsLabel(1, 1)).toBe('1 hit, 1 miss');
    expect(asciiOnly(cacheStatsLabel(3, 1))).toBe(true);
  });

  it('metadata progress and cache counters share one label', () => {
    expect(metadataProgressLabel(0, 0, 0, 0)).toBe('');
    expect(metadataProgressLabel(4, 9, 0, 0)).toBe('metadata: 4/9');
    expect(metadataProgressLabel(0, 0, 2, 1)).toBe('2 hits, 1 miss');
    expect(metadataProgressLabel(4, 9, 2, 1)).toBe(
      'metadata: 4/9, 2 hits, 1 miss',
    );
    expect(asciiOnly(metadataProgressLabel(9, 9, 12, 0))).toBe(true);
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