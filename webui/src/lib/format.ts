// Display formatting helpers shared by components. ASCII only, matching
// the house style and the TUI's rendering conventions.

import type { ReportEntry, Suggestion } from '../types';

/** The last `n` path segments joined by '/', or the whole path when shorter. */
export function tail(path: string, n: number): string {
  const trimmed = path.trimEnd();
  if (!trimmed) return '/';
  const parts = trimmed.split('/').filter((p) => p.length > 0);
  if (parts.length <= n) {
    return `/${parts.join('/')}`;
  }
  return `/${parts.slice(parts.length - n).join('/')}`;
}

/** Elide the middle of a long string, keeping a short tail. */
export function shorten(s: string, max: number): string {
  if (s.length <= max) return s;
  const keep = Math.max(0, max - 3);
  return `...${s.slice(s.length - keep)}`;
}

export function rowLabel(s: Suggestion): string {
  const marker = s.selected ? '[x]' : '[ ]';
  const name = tail(s.target, 3);
  const dest = s.new_path ? tail(s.new_path, 3) : 'delete';
  const status = s.status === 'pending' ? '' : ` (${s.status})`;
  const rejected = s.rejection_reason ? ' [rejected]' : '';
  return `${marker} ${s.type.padEnd(6)} ${s.asset_kind.padEnd(8)} ${name} -> ${dest}${status}${rejected}`;
}

export interface ReportCounts {
  executed: number;
 failed: number; skipped: number;
}

export function countsOf(entries: ReportEntry[]): ReportCounts {
  const counts: ReportCounts = { executed: 0, failed: 0, skipped: 0 };
  for (const e of entries) {
    switch (e.result) {
      case 'executed':
        counts.executed += 1;
        break;
      case 'failed':
        counts.failed += 1;
        break;
      case 'skipped':
        counts.skipped += 1;
        break;
    }
  }
  return counts;
}

export function countsSummary(counts: ReportCounts): string {
  const parts: string[] = [];
  if (counts.executed > 0) parts.push(`executed=${counts.executed}`);
  if (counts.failed > 0) parts.push(`failed=${counts.failed}`);
  if (counts.skipped > 0) parts.push(`skipped=${counts.skipped}`);
  return parts.join('  ') || 'no entries executed';
}

/** One display line for a report entry (top-level result only). */
export function entryLine(e: ReportEntry): string {
  const s = e.suggestion;
  if (s?.target && s.new_path) {
    return `[${e.result}] ${s.target} -> ${s.new_path}`;
  }
  return `[${e.result}] ${e.suggestion_id}`;
}

/** One display line for an in-flight `execution.op` SSE event. */
export function opLine(op: {
  suggestion_id: string;
  operation: string;
  status: string;
  detail?: unknown;
}): string {
  const suffix = op.detail ? ` (${String(op.detail)})` : '';
  return `[${op.status}] ${op.operation} ${op.suggestion_id}${suffix}`;
}

export function elapsedLabel(ms: number): string {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  const r = s % 60;
  const secs = String(r).padStart(2, '0');
  return m > 0 ? m + 'm' + secs + 's' : s + 's';
}



export function stateLabel(state: string): string {
  const map: Record<string, string> = {
    idle: 'idle',
    scanning: 'scanning',
    analyzing: 'analyzing',
    ready: 'ready',
    executing: 'executing',
    done: 'done',
    failed: 'failed',
  };
  return map[state] ?? state;
}

export function asciiOnly(s: string): boolean {
  return s.split('').every((c) => c.charCodeAt(0) <= 127);
}