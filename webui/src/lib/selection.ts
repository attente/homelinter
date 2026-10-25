// Client-side application of the selection reconciliation returned by the
// service (see DESIGN.md "API and Service"). Besides the authoritative
// selection IDs, the response carries suggestions revalidation rejected, so
// stale rows can be corrected without refetching.

import type { SelectionResult, Suggestion } from '../types';

/** Apply a service selection result to `suggestions`: every row's `selected`
 *  flag matches the returned selection, and every reported rejection updates
 *  the row's status and reason. Unchanged rows keep their identity. */
export function applySelectionResult(
  suggestions: Suggestion[],
  result: SelectionResult,
): Suggestion[] {
  const selected = new Set(result.ids);
  const reasons = new Map(result.rejected.map((r) => [r.id, r.reason]));
  let changed = false;
  const next = suggestions.map((s) => {
    const want = selected.has(s.id);
    const reason = reasons.get(s.id);
    if (reason === undefined && s.selected === want) return s;
    changed = true;
    if (reason !== undefined) {
      return {
        ...s,
        selected: want,
        status: 'rejected' as const,
        rejection_reason: reason,
      };
    }
    return { ...s, selected: want };
  });
  return changed ? next : suggestions;
}
