// Client-side filter logic. Filters affect navigation and bulk selection only;
// the service keeps the authoritative selection (see DESIGN.md "User
// Interfaces"). Pure functions so they are unit-testable without a DOM.

import type { Suggestion, SuggestionStatus, SuggestionType } from '../types';

/** One choice in a filter combo box. `all` is the unconstrained value. */
export interface FilterOption<T extends string> {
  value: T;
  label: string;
}

export const TYPE_OPTIONS: ReadonlyArray<FilterOption<'all' | SuggestionType>> = [
  { value: 'all', label: 'move or delete' },
  { value: 'move', label: 'move' },
  { value: 'delete', label: 'delete' },
];

export const STATUS_OPTIONS: ReadonlyArray<FilterOption<'all' | SuggestionStatus>> = [
  { value: 'all', label: 'any status' },
  { value: 'pending', label: 'pending' },
  { value: 'rejected', label: 'rejected' },
  { value: 'executed', label: 'done' },
  { value: 'partial', label: 'partial' },
  { value: 'failed', label: 'failed' },
  { value: 'skipped', label: 'skipped' },
];

export const MANUAL_OPTIONS: ReadonlyArray<FilterOption<'all' | 'manual'>> = [
  { value: 'all', label: 'auto or manual' },
  { value: 'manual', label: 'manual' },
];

export interface Filter {
  type: 'all' | SuggestionType;
  kind: 'all' | string;
  status: 'all' | SuggestionStatus;
  /** `manual` keeps only suggestions backed by a saved manual override. */
  manual: 'all' | 'manual';
  /** Case-insensitive substring matched against the suggestion's path
   *  fields (source target, move destination, duplicate keeper). Absent or
   *  empty is unconstrained. Mirrors the service's `path` filter parameter. */
  path?: string;
}

export const ALL_FILTER: Filter = {
  type: 'all',
  kind: 'all',
  status: 'all',
  manual: 'all',
  path: '',
};

/** True when no dimension is constrained, so `clear` has nothing to do. */
export function isDefaultFilter(f: Filter): boolean {
  return (
    f.type === ALL_FILTER.type &&
    f.kind === ALL_FILTER.kind &&
    f.status === ALL_FILTER.status &&
    f.manual === ALL_FILTER.manual &&
    (f.path ?? '') === ''
  );
}

/** The suggestion's path values a path filter matches against: the source
 *  target, a move's destination, and a duplicate delete's retained keeper.
 *  Matches the service-side field list (see the model's
 *  `path_filter_matches`). Paths are matched in full, not in the truncated
 *  display form, so a hit is never hidden by row truncation. */
function suggestionPaths(s: Suggestion): string[] {
  const dup = s.metadata?.duplicate_of;
  return [
    s.target,
    s.new_path ?? undefined,
    typeof dup === 'string' && dup.length > 0 ? dup : undefined,
  ].filter((p): p is string => typeof p === 'string' && p.length > 0);
}

/** Case-insensitive substring test over a suggestion's path fields; an
 *  empty needle is unconstrained. Must stay identical to the service-side
 *  filter semantics so streamed deltas match the paged server window. */
export function pathMatches(s: Suggestion, needle: string): boolean {
  const n = needle.toLowerCase();
  if (n === '') return true;
  return suggestionPaths(s).some((p) => p.toLowerCase().includes(n));
}

export function matchesFilter(s: Suggestion, f: Filter): boolean {
  if (f.type !== 'all' && s.type !== f.type) {
    return false;
  }
  if (f.kind !== 'all' && s.asset_kind !== f.kind) {
    return false;
  }
  if (f.status !== 'all' && s.status !== f.status) {
    return false;
  }
  if (f.manual === 'manual' && (s.manual_overrides?.length ?? 0) === 0) {
    return false;
  }
  if (f.path && !pathMatches(s, f.path)) {
    return false;
  }
  return true;
}

/** Read the filter from the page URL's query parameters, so a reload or a
 *  shared link restores the filtered view. Missing or unrecognized values
 *  fall back to the unconstrained defaults. */
export function filterFromSearchParams(params: URLSearchParams): Filter {
  const type = params.get('type');
  const kind = params.get('kind');
  const status = params.get('status');
  const manual = params.get('manual');
  const path = params.get('path');
  const typeValue = TYPE_OPTIONS.find((o) => o.value === type)?.value;
  const statusValue = STATUS_OPTIONS.find((o) => o.value === status)?.value;
  return {
    type: typeValue === 'move' || typeValue === 'delete' ? typeValue : 'all',
    kind: kind && kind !== 'all' ? kind : 'all',
    status:
      statusValue && statusValue !== 'all' ? statusValue : 'all',
    manual: manual === 'manual' ? 'manual' : 'all',
    path: path ?? '',
  };
}

/** Serialize a filter into query parameters for the page URL; unconstrained
 *  dimensions are omitted, so a fully cleared filter yields no query string. */
export function filterToSearchParams(f: Filter): URLSearchParams {
  const q = new URLSearchParams();
  if (f.type !== 'all') q.set('type', f.type);
  if (f.kind !== 'all') q.set('kind', f.kind);
  if (f.status !== 'all') q.set('status', f.status);
  if (f.manual !== 'all') q.set('manual', f.manual);
  if (f.path) q.set('path', f.path);
  return q;
}

export interface SuggestionMerge {
  suggestions: Suggestion[];
  changed: boolean;
}

/**
 * Apply streamed `suggestion.updated` / `suggestion.removed` events to the
 * loaded window while respecting the active filter. A new row is appended
 * only when it matches the filter, and an update that pushes a loaded row out
 * of the filter (for example a pending row that re-validates to rejected)
 * drops it. Without this, streamed metadata resolutions leak rows into the
 * list that the server-side filter would have excluded.
 */
export function mergeSuggestionEvents(
  current: Suggestion[],
  updates: Map<string, Suggestion>,
  removals: Set<string>,
  filter: Filter,
): SuggestionMerge {
  let changed = false;
  let next = current;
  if (updates.size > 0) {
    const unmatched = new Set(updates.keys());
    next = next.flatMap((s) => {
      const item = updates.get(s.id);
      if (!item) return [s];
      unmatched.delete(s.id);
      if (item === s) return [s];
      changed = true;
      return matchesFilter(item, filter) ? [item] : [];
    });
    for (const id of unmatched) {
      const item = updates.get(id);
      if (item && matchesFilter(item, filter)) {
        next.push(item);
        changed = true;
      }
    }
  }
  if (removals.size > 0 && next.some((s) => removals.has(s.id))) {
    next = next.filter((s) => !removals.has(s.id));
    changed = true;
  }
  return { suggestions: next, changed };
}

export function uniqueKinds(items: Suggestion[]): string[] {
  const seen = new Set<string>();
  for (const s of items) {
    if (s.asset_kind) seen.add(s.asset_kind);
  }
  return [...seen].sort();
}
