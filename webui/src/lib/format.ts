// Display formatting helpers shared by components. ASCII only, matching
// the house style and the TUI's rendering conventions.

import type { ReportEntry, Suggestion } from '../types';

/**
 * A path as shown in a suggestion row: made relative to the scan root when
 * it lives under it, kept verbatim up to three segments, and truncated to
 * the last three segments with an ellipsis prefix beyond that. Paths
 * outside the scan root (e.g. a destination resolved elsewhere under
 * $HOME) fall back to the absolute path under the same truncation rule.
 */
export function pathLabel(path: string, root?: string | null): string {
  const rel = relativeToScanRoot(path, root);
  const parts = rel.split('/').filter((p) => p.length > 0);
  if (parts.length <= 3) {
    return rel;
  }
  return `.../${parts.slice(parts.length - 3).join('/')}`;
}

/** Strip the scan root prefix when `path` is under it; else the path as-is. */
function relativeToScanRoot(path: string, root?: string | null): string {
  const base = root?.trimEnd().replace(/\/+$/, '');
  if (!base || base === '/') {
    return path;
  }
  if (path === base) {
    return '/';
  }
  if (path.startsWith(`${base}/`)) {
    return path.slice(base.length + 1);
  }
  return path;
}

/** Elide the middle of a long string, keeping a short tail. */
export function shorten(s: string, max: number): string {
  if (s.length <= max) return s;
  const keep = Math.max(0, max - 3);
  return `...${s.slice(s.length - keep)}`;
}

/** Last path segment, for the compact "scanning <filename>" placeholder. */
function fileName(path: string): string {
  const part = path.split('/').pop();
  return part && part.length > 0 ? part : path;
}

export function rowLabel(s: Suggestion, root?: string | null): string {
  const marker = s.selected ? '[x]' : '[ ]';
  // A suggestion whose provider metadata is still in flight has no stable
  // destination yet, so it is shown as an in-progress scan of its file.
  if (s.metadata_pending) {
    return `${marker} scanning ${fileName(s.target)}`;
  }
  const name = pathLabel(s.target, root);
  let action: string;
  if (s.type === 'delete') {
    const dupOf = s.metadata?.duplicate_of;
    action = typeof dupOf === 'string' && dupOf.length > 0
      ? `delete ${name}, duplicate of ${pathLabel(dupOf, root)}`
      : `delete ${name}`;
  } else {
    const dest = s.new_path ? pathLabel(s.new_path, root) : '?';
    action = `move ${name} to ${dest}`;
  }
  const status = s.status === 'pending' ? '' : ` (${s.status})`;
  const rejected = s.rejection_reason
    ? ` [rejected: ${head(s.rejection_reason, 48)}]`
    : '';
  return `${marker} ${action}${status}${rejected}`;
}

/** rowLabel without the leading selection marker, for use next to a real checkbox. */
export function rowText(s: Suggestion, root?: string | null): string {
  return rowLabel(s, root).slice(4);
}

/** Human-readable list of the fields backed by manual overrides. */
export function manualOverrideLabel(
  fields: readonly string[] | null | undefined,
): string {
  if (!fields || fields.length === 0) return '';
  if (fields.length === 1) return fields[0];
  if (fields.length === 2) return `${fields[0]} and ${fields[1]}`;
  return `${fields.slice(0, -1).join(', ')}, and ${fields[fields.length - 1]}`;
}

/** Keep the head of a string, truncating the tail when too long. */
export function head(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max - 3)}...`;
}

export interface ReportCounts {
  executed: number;
  partial: number;
  failed: number;
  skipped: number;
}

export function countsOf(entries: ReportEntry[]): ReportCounts {
  const counts: ReportCounts = { executed: 0, partial: 0, failed: 0, skipped: 0 };
  for (const e of entries) {
    switch (e.result) {
      case 'executed':
        counts.executed += 1;
        break;
      case 'partial':
        counts.partial += 1;
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
  if (counts.partial > 0) parts.push(`partial=${counts.partial}`);
  if (counts.failed > 0) parts.push(`failed=${counts.failed}`);
  if (counts.skipped > 0) parts.push(`skipped=${counts.skipped}`);
  return parts.join('  ') || 'no entries executed';
}

/** One display line for a report entry (top-level result only). */
export function entryLine(e: ReportEntry): string {
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

/**
 * One scan-progress line for the scanning view, e.g.
 * `metadata (exiftool) 4312/11263, elapsed=12s`. The total is shown as
 * `scanned/total` when the phase knows it up front and as `scanned=N`
 * otherwise; the detail names the worker filling the phase (for example an
 * extractor) when the server reports one.
 */
export function scanProgressLabel(p: {
  phase: string;
  scanned: number;
  total: number | null;
  detail: string | null;
  elapsed_ms: number;
}): string {
  const detail = p.detail ? ` (${p.detail})` : '';
  const count =
    p.total != null ? `${p.scanned}/${p.total}` : `scanned=${p.scanned}`;
  return `${p.phase}${detail} ${count}, elapsed=${elapsedLabel(p.elapsed_ms)}`;
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

/**
 * Header label for the provider cache counters, e.g. `12 hits, 3 misses`.
 * The two counts are named rather than slashed because they are not a
 * fraction of one another: a hit is a request the cache answered, a miss is
 * one that had to reach the provider.
 */
export function cacheStatsLabel(hits: number, misses: number): string {
  const h = `${hits} ${hits === 1 ? 'hit' : 'hits'}`;
  const m = `${misses} ${misses === 1 ? 'miss' : 'misses'}`;
  return `${h}, ${m}`;
}

/**
 * Metadata progress and provider-cache counters share one label,
 * e.g. `metadata: 4/9, 3 hits, 6 misses`, shown in the browsing view's
 * activity strip above the list and inline in the scanning view. A hit or a
 * miss only means something next to the progress it advanced, and one
 * combined label keeps the view from growing a counter per phase. Empty when
 * there is nothing to report yet.
 */
export function metadataProgressLabel(
  completed: number,
  total: number,
  hits: number,
  misses: number,
): string {
  const parts: string[] = [];
  if (total > 0) parts.push(`metadata: ${completed}/${total}`);
  if (hits + misses > 0) parts.push(cacheStatsLabel(hits, misses));
  return parts.join(', ');
}
/**
 * Count label for the filter row: `1234 suggestions` while no dimension is
 * constrained, `42 of 1234 suggestions` while the list shows a filtered
 * subset of the batch.
 */
export function suggestionCountLabel(
  total: number,
  matched: number,
  filtered: boolean,
): string {
  const noun = total === 1 ? 'suggestion' : 'suggestions';
  return filtered ? `${matched} of ${total} ${noun}` : `${total} ${noun}`;
}
