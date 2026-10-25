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
  appendScanStage,
  cacheStatsLabel,
  scanProgressFraction,
  scanProgressLabel,
  scanProgressPercent,
  suggestionCountLabel,
  countsOf,
  countsSummary,
  countsSummaryParts,
  statusLabel,
  actionLabel,
  actionLabelParts,
  actionText,
  isSelectable,
  manualOverrideLabel,
  metadataProgressLabel,
  pathLabel,
  rowLabel,
  rowText,
  rowTone,
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
  it('renders the scan progress line with total and worker detail', () => {
    expect(
      scanProgressLabel({
        phase: 'metadata',
        scanned: 4312,
        total: 11263,
        detail: 'exiftool',
        elapsed_ms: 12500,
      }),
    ).toBe('metadata (exiftool) 4312/11263, elapsed=12s');
    expect(
      scanProgressLabel({
        phase: 'discovery',
        scanned: 7,
        total: null,
        detail: null,
        elapsed_ms: 0,
      }),
    ).toBe('discovery scanned=7, elapsed=0s');
    // Old events without the newer optional fields degrade gracefully.
    expect(
      scanProgressLabel({
        phase: 'hashing',
        scanned: 9000,
        total: null,
        detail: undefined as unknown as null,
        elapsed_ms: 30000,
      }),
    ).toBe('hashing scanned=9000, elapsed=30s');
  });

  it('turns a scan progress event into a meter fraction', () => {
    // Known totals fill the bar.
    expect(scanProgressFraction({ scanned: 0, total: 4000 })).toBe(0);
    expect(scanProgressFraction({ scanned: 1000, total: 4000 })).toBe(0.25);
    expect(scanProgressFraction({ scanned: 4000, total: 4000 })).toBe(1);
    expect(scanProgressPercent({ scanned: 1000, total: 4000 })).toBe(25);
    expect(scanProgressPercent({ scanned: 11263, total: 11263 })).toBe(100);
    // Overrun and zero totals never produce a bar outside the track.
    expect(scanProgressFraction({ scanned: 5, total: 4 })).toBe(1);
    expect(scanProgressFraction({ scanned: 5, total: 0 })).toBeNull();
    // Phases that discover their length as they run report no total, so the
    // meter has to stay indeterminate rather than invent a percentage.
    expect(scanProgressFraction({ scanned: 929280, total: null })).toBeNull();
    expect(scanProgressPercent({ scanned: 929280, total: null })).toBeNull();
  });

  it('folds scan progress events into persisted stage rows', () => {
    // The first event opens the first row.
    let rows = appendScanStage([], {
      phase: 'discovery',
      scanned: 1,
      total: null,
      detail: null,
      elapsed_ms: 1000,
    });
    expect(rows).toEqual([
      {
        phase: 'discovery',
        scanned: 1,
        total: null,
        detail: null,
        elapsed_ms: 1000,
        complete: false,
      },
    ]);
    // The heartbeat's republication of the same phase updates the row in
    // place instead of appending a duplicate stage.
    rows = appendScanStage(rows, {
      phase: 'discovery',
      scanned: 7,
      total: null,
      detail: null,
      elapsed_ms: 2000,
    });
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ phase: 'discovery', scanned: 7 });
    // A new phase completes the previous row (frozen at its last counts) and
    // appends the new active row at the end.
    rows = appendScanStage(rows, {
      phase: 'symlinks',
      scanned: 3,
      total: 3,
      detail: null,
      elapsed_ms: 3000,
    });
    expect(rows).toEqual([
      {
        phase: 'discovery',
        scanned: 7,
        total: null,
        detail: null,
        elapsed_ms: 2000,
        complete: true,
      },
      {
        phase: 'symlinks',
        scanned: 3,
        total: 3,
        detail: null,
        elapsed_ms: 3000,
        complete: false,
      },
    ]);
    // A phase returning after other phases ran in between starts a new row;
    // only the last row is active, every row before it is complete.
    rows = appendScanStage(rows, {
      phase: 'metadata',
      scanned: 10,
      total: 10,
      detail: 'exiftool',
      elapsed_ms: 4000,
    });
    rows = appendScanStage(rows, {
      phase: 'hashing',
      scanned: 1,
      total: 1,
      detail: null,
      elapsed_ms: 5000,
    });
    rows = appendScanStage(rows, {
      phase: 'metadata',
      scanned: 10,
      total: 10,
      detail: null,
      elapsed_ms: 6000,
    });
    expect(rows.map((r) => r.phase)).toEqual([
      'discovery',
      'symlinks',
      'metadata',
      'hashing',
      'metadata',
    ]);
    expect(rows.slice(0, -1).every((r) => r.complete)).toBe(true);
    expect(rows[rows.length - 1].complete).toBe(false);
  });

  it('labels the filter-row suggestion counts', () => {
    expect(suggestionCountLabel(1234, 1234, false)).toBe('1234 suggestions');
    expect(suggestionCountLabel(1, 1, false)).toBe('1 suggestion');
    expect(suggestionCountLabel(1234, 42, true)).toBe('42 of 1234 suggestions');
  });

  it('shares the filter-row label with the manual-override count', () => {
    expect(suggestionCountLabel(1234, 1234, false, 17)).toBe(
      '1234 suggestions, 17 manual',
    );
    expect(suggestionCountLabel(1234, 42, true, 0)).toBe(
      '42 of 1234 suggestions, 0 manual',
    );
  });

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

  it('row label shows the rejection reason when present, without a status suffix', () => {
    const s = sampleSuggestion({
      status: 'rejected',
      rejection_reason: 'destination_collision:also=abc123',
    });
    const label = rowLabel(s);
    expect(label).toContain('[rejected: destination_collision:also=abc123]');
    expect(label).not.toContain('(rejected)');
    const plain = rowLabel(sampleSuggestion({ status: 'rejected' }));
    expect(plain).not.toContain('[rejected:');
  });

  it('no row label appends the outcome in parentheses', () => {
    expect(rowLabel(sampleSuggestion({ status: 'pending' }))).not.toContain('(');
    for (const status of ['executed', 'partial', 'failed', 'skipped'] as const) {
      const label = rowText(sampleSuggestion({ status }), '/home/a');
      expect(label).not.toMatch(/\(/);
      expect(label).not.toContain(statusLabel(status));
    }
  });

  it('row tone maps each finished outcome to its colour class', () => {
    expect(rowTone('pending')).toBeNull();
    expect(rowTone('executed')).toBe('done');
    expect(rowTone('partial')).toBe('partial');
    expect(rowTone('failed')).toBe('bad');
    expect(rowTone('rejected')).toBe('bad');
    expect(rowTone('skipped')).toBe('skipped');
  });

  it('only pending-like outcomes are selectable', () => {
    for (const status of [
      'pending',
      'partial',
      'failed',
      'skipped',
    ] as const) {
      expect(isSelectable({ status })).toBe(true);
    }
    expect(isSelectable({ status: 'executed' })).toBe(false);
    expect(isSelectable({ status: 'rejected' })).toBe(false);
  });

  it('action label reads the performed operation, not the id', () => {
    const root = '/home/a';
    expect(actionLabel(sampleSuggestion(), root)).toBe(
      'move IMG_1.jpg to Pictures/2024/IMG_1.jpg',
    );
    expect(
      actionLabel(
        sampleSuggestion({
          type: 'delete',
          new_path: null,
          metadata: { duplicate_of: '/home/a/keeper/IMG_1.jpg' },
        }),
        root,
      ),
    ).toBe('delete IMG_1.jpg, duplicate of keeper/IMG_1.jpg');
    expect(
      actionLabel(sampleSuggestion({ type: 'delete', new_path: null }), root),
    ).toBe('delete IMG_1.jpg');
  });

  it('action parts split words from paths for the run list', () => {
    const parts = actionLabelParts(sampleSuggestion(), '/home/a');
    expect(parts).toEqual([
      { kind: 'text', text: 'move ' },
      { kind: 'path', text: 'IMG_1.jpg', path: '/home/a/IMG_1.jpg' },
      { kind: 'text', text: ' to ' },
      {
        kind: 'path',
        text: 'Pictures/2024/IMG_1.jpg',
        path: '/home/a/Pictures/2024/IMG_1.jpg',
      },
    ]);
    expect(actionText(parts)).toBe(actionLabel(sampleSuggestion(), '/home/a'));
    expect(
      actionLabelParts(
        sampleSuggestion({
          type: 'delete',
          new_path: null,
          metadata: { duplicate_of: '/home/a/keeper/IMG_1.jpg' },
        }),
        '/home/a',
      ).map((p) => p.kind),
    ).toEqual(['text', 'path', 'text', 'path']);
  });

  it('path parts carry the full path for copying', () => {
    const move = actionLabelParts(sampleSuggestion(), '/home/a');
    expect(move.filter((p) => p.kind === 'path').map((p) => p.path)).toEqual([
      '/home/a/IMG_1.jpg',
      '/home/a/Pictures/2024/IMG_1.jpg',
    ]);
    const dup = actionLabelParts(
      sampleSuggestion({
        type: 'delete',
        new_path: null,
        metadata: { duplicate_of: '/home/a/keeper/IMG_1.jpg' },
      }),
      '/home/a',
    );
    expect(dup.filter((p) => p.kind === 'path').map((p) => p.path)).toEqual([
      '/home/a/IMG_1.jpg',
      '/home/a/keeper/IMG_1.jpg',
    ]);
    // A missing destination has no full path to copy, only the "?" display.
    const unknown = actionLabelParts(
      sampleSuggestion({ new_path: null }),
      '/home/a',
    );
    expect(unknown.at(-1)).toEqual({ kind: 'path', text: '?', path: undefined });
  });

  it('action parts use the past tense for the execution list', () => {
    expect(
      actionText(actionLabelParts(sampleSuggestion(), '/home/a', { past: true })),
    ).toBe('moved IMG_1.jpg to Pictures/2024/IMG_1.jpg');
    expect(
      actionText(
        actionLabelParts(
          sampleSuggestion({
            type: 'delete',
            new_path: null,
            metadata: { duplicate_of: '/home/a/keeper/IMG_1.jpg' },
          }),
          '/home/a',
          { past: true },
        ),
      ),
    ).toBe('deleted IMG_1.jpg, duplicate of keeper/IMG_1.jpg');
    // The browser's proposal rows keep the imperative form.
    expect(actionLabel(sampleSuggestion(), '/home/a')).toContain('move ');
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
    expect(countsSummary(countsOf(entries))).toContain('done=1');
    expect(countsSummary(countsOf(entries)).includes('partial=1')).toBe(true);
    expect(countsSummary({ executed: 0, partial: 0, failed: 0, skipped: 0 })).toBe(
      'no actions done',
    );
    expect(countsSummaryParts(countsOf(entries))).toEqual([
      { label: 'done=1', tone: 'ok' },
      { label: 'partial=1', tone: 'busy' },
      { label: 'failed=1', tone: 'bad' },
      { label: 'skipped=1', tone: 'dim' },
    ]);
    expect(countsSummaryParts({ executed: 3, partial: 0, failed: 0, skipped: 2 })).toEqual([
      { label: 'done=3', tone: 'ok' },
      { label: 'skipped=2', tone: 'dim' },
    ]);
    expect(countsSummaryParts({ executed: 0, partial: 0, failed: 0, skipped: 0 })).toEqual([
      { label: 'no actions done', tone: 'dim' },
    ]);
    expect(statusLabel('executed')).toBe('done');
    expect(statusLabel('partial')).toBe('partial');
  });
});