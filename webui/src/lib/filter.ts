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
  { value: 'all', label: 'any type' },
  { value: 'move', label: 'move' },
  { value: 'delete', label: 'delete' },
];

export const STATUS_OPTIONS: ReadonlyArray<FilterOption<'all' | SuggestionStatus>> = [
  { value: 'all', label: 'any status' },
  { value: 'pending', label: 'pending' },
  { value: 'rejected', label: 'rejected' },
  { value: 'executed', label: 'executed' },
  { value: 'partial', label: 'partial' },
  { value: 'failed', label: 'failed' },
  { value: 'skipped', label: 'skipped' },
];

export const MANUAL_OPTIONS: ReadonlyArray<FilterOption<'all' | 'manual'>> = [
  { value: 'all', label: 'any metadata' },
  { value: 'manual', label: 'manual only' },
];

export interface Filter {
  type: 'all' | SuggestionType;
  kind: 'all' | string;
  status: 'all' | SuggestionStatus;
  /** `manual` keeps only suggestions backed by a saved manual override. */
  manual: 'all' | 'manual';
}

export const ALL_FILTER: Filter = {
  type: 'all',
  kind: 'all',
  status: 'all',
  manual: 'all',
};

/** True when no dimension is constrained, so `clear` has nothing to do. */
export function isDefaultFilter(f: Filter): boolean {
  return (
    f.type === ALL_FILTER.type &&
    f.kind === ALL_FILTER.kind &&
    f.status === ALL_FILTER.status &&
    f.manual === ALL_FILTER.manual
  );
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
  return true;
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
