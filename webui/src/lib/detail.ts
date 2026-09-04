// Detail pane line pairs for a suggestion. Pure function, testable.

import type { Suggestion } from '../types';

export type DetailLine = [string, string];

export function detailLines(s: Suggestion): DetailLine[] {
  const out: DetailLine[] = [];
  out.push(['id', s.id]);
  out.push(['type', s.type]);
  out.push(['asset kind', s.asset_kind]);
  out.push(['status', s.status]);
  out.push(['target', s.target]);
  if (s.new_path) out.push(['new path', s.new_path]);
  if (s.reason) out.push(['reason', s.reason]);
  for (const d of (s.dependent_operations ?? [])) {
    out.push(['dependent', opLabel(d)]);
  }
  if (s.rejection_reason) out.push(['rejected', s.rejection_reason]);
  return out;
}

function opLabel(d: Record<string, unknown>): string {
  const entries = Object.entries(d).filter(([k]) => k !== 'type');
  const rest = entries.map(([k, v]) => `${k}=${String(v)}`).sort();
  return rest.length ? `${d.type} (${rest.join(', ')})` : `${d.type}`;
}