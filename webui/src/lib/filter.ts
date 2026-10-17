// Client-side filter logic. Filters affect navigation and bulk selection only;
// the service keeps the authoritative selection (see DESIGN.md "User
// Interfaces"). Pure functions so they are unit-testable without a DOM.

import type { Suggestion, SuggestionStatus, SuggestionType } from '../types';

export const TYPE_CYCLE: ReadonlyArray<'all' | SuggestionType> = [
  'all',
  'move',
  'delete',
];

export const STATUS_CYCLE: ReadonlyArray<'all' | SuggestionStatus> = [
  'all',
  'pending',
  'rejected',
  'executed',
  'failed',
  'skipped',
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

export const MANUAL_CYCLE: ReadonlyArray<'all' | 'manual'> = ['all', 'manual'];

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

/** Move to the next value in a cycle, wrapping around. */
export function cycleNext<T extends string>(cycle: ReadonlyArray<T>, current: T): T {
  const idx = cycle.indexOf(current);
  const next = cycle[(idx + 1) % cycle.length] ?? cycle[0];
  return next;
}