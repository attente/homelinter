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
}

export const ALL_FILTER: Filter = { type: 'all', kind: 'all', status: 'all' };

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
  return true;
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