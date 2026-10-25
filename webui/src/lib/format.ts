// Display formatting helpers shared by components. ASCII only, matching
// the house style and the TUI's rendering conventions.

import type {
  ReportEntry,
  ScanProgress,
  ScanStage,
  Suggestion,
  SuggestionStatus,
} from '../types';

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

/** One display piece of an action sentence: a connecting word, or a path
 *  that the run list renders in the monospace stack. */
export interface ActionPart {
  kind: 'text' | 'path';
  /** Display text: a path is scan-root relative and may be truncated. */
  text: string;
  /** The full absolute path a path part stands for, so clicking it copies
   *  what the shortened display cannot show. Absent on text parts. */
  path?: string;
}

export interface ActionLabelOptions {
  /** Past tense (`moved`, `deleted`) for historical contexts: the execution
   *  list and the downloaded report describe what happened, while the
   *  browser's rows describe what a suggestion would do. */
  past?: boolean;
}

/** The action sentence for a suggestion as display parts: `move <path> to
 *  <path>` for moves and `delete <path>, duplicate of <path>` for duplicate
 *  deletes (the retained keeper read from the suggestion's
 *  `duplicate_of` metadata), or `moved`/`deleted` with `{ past: true }`. The
 *  parts let the run view keep the words in the UI typeface while the paths
 *  carry the monospace stack. */
export function actionLabelParts(
  s: Suggestion,
  root?: string | null,
  options?: ActionLabelOptions,
): ActionPart[] {
  const past = options?.past === true;
  const name = pathLabel(s.target, root);
  if (s.type === 'delete') {
    const dupOf = s.metadata?.duplicate_of;
    if (typeof dupOf === 'string' && dupOf.length > 0) {
      return [
        { kind: 'text', text: past ? 'deleted ' : 'delete ' },
        { kind: 'path', text: name, path: s.target },
        { kind: 'text', text: ', duplicate of ' },
        { kind: 'path', text: pathLabel(dupOf, root), path: dupOf },
      ];
    }
    return [
      { kind: 'text', text: past ? 'deleted ' : 'delete ' },
      { kind: 'path', text: name, path: s.target },
    ];
  }
  return [
    { kind: 'text', text: past ? 'moved ' : 'move ' },
    { kind: 'path', text: name, path: s.target },
    { kind: 'text', text: ' to ' },
    {
      kind: 'path',
      text: s.new_path ? pathLabel(s.new_path, root) : '?',
      path: s.new_path ?? undefined,
    },
  ];
}

/** The action sentence as one string, shared by the browse rows. */
export function actionLabel(s: Suggestion, root?: string | null): string {
  return actionText(actionLabelParts(s, root));
}

/** Join action parts back into their plain sentence (titles, test labels). */
export function actionText(parts: readonly ActionPart[]): string {
  return parts.map((p) => p.text).join('');
}

export function rowLabel(s: Suggestion, root?: string | null): string {
  const marker = s.selected ? '[x]' : '[ ]';
  // A suggestion whose provider metadata is still in flight has no stable
  // destination yet, so it is shown as an in-progress scan of its file.
  if (s.metadata_pending) {
    return `${marker} scanning ${fileName(s.target)}`;
  }
  const action = actionLabel(s, root);
  const rejected = s.rejection_reason
    ? ` [rejected: ${head(s.rejection_reason, 48)}]`
    : '';
  return `${marker} ${action}${rejected}`;
}

/** rowLabel without the leading selection marker, for use next to a real checkbox. */
export function rowText(s: Suggestion, root?: string | null): string {
  return rowLabel(s, root).slice(4);
}

/**
 * The outcome tone of a browser row, carried as a class instead of a status
 * suffix: a done row reads green and struck through, red marks rejected and
 * failed rows, orange marks a partial run, and skipped rows stay dim. A
 * pending (or still metadata-pending) row has no tone and keeps the normal
 * text colour.
 */
export type RowTone = 'done' | 'bad' | 'partial' | 'skipped';

/** A rejected or executed suggestion is never selectable. */
export function isSelectable(s: Pick<Suggestion, 'status'>): boolean {
  return s.status !== 'executed' && s.status !== 'rejected';
}

export function rowTone(status: SuggestionStatus): RowTone | null {
  switch (status) {
    case 'executed':
      return 'done';
    case 'rejected':
    case 'failed':
      return 'bad';
    case 'partial':
      return 'partial';
    case 'skipped':
      return 'skipped';
    default:
      return null;
  }
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

/** The SPA's display word for an internal execution status: a successful
 *  execution is shown as `done`, while the API keeps using `executed`. */
export function statusLabel(status: string): string {
  return status === 'executed' ? 'done' : status;
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

export type CountsTone = 'ok' | 'busy' | 'bad' | 'dim';

/** The summary counts as labelled parts, each carrying the outcome tone its
 *  run rows use, so the header can colour the counts by goodness. */
export function countsSummaryParts(
  counts: ReportCounts,
): Array<{ label: string; tone: CountsTone }> {
  const halves: Array<[string, number, CountsTone]> = [
    ['done', counts.executed, 'ok'],
    ['partial', counts.partial, 'busy'],
    ['failed', counts.failed, 'bad'],
    ['skipped', counts.skipped, 'dim'],
  ];
  const parts = halves
    .filter(([, n]) => n > 0)
    .map(([word, n, tone]) => ({ label: `${word}=${n}`, tone }));
  return parts.length > 0 ? parts : [{ label: 'no actions done', tone: 'dim' }];
}

export function countsSummary(counts: ReportCounts): string {
  return countsSummaryParts(counts)
    .map((part) => part.label)
    .join('  ');
}

/** A byte count in binary units, matching the Rust clients' label. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit + 1 < units.length) {
    value /= 1024;
    unit += 1;
  }
  const text =
    unit === 0 || value >= 10 ? value.toFixed(0) : value.toFixed(1);
  return `${text} ${units[unit]}`;
}

/** The recovered-space part of an execution summary, or an empty string when
 *  no duplicate delete reclaimed anything. */
export function reclaimedLabel(bytes: number): string {
  return bytes > 0 ? `reclaimed ${formatBytes(bytes)}` : '';
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


/**
 * Fraction of the current phase's work that is done, in `0..=1`, or null when
 * the phase cannot know its total up front (the discovery and symlinks walks,
 * and the hashing confirmation pass). Drives the progress meter, so it clamps
 * rather than reporting a bar that overflows.
 */
export function scanProgressFraction(p: {
  scanned: number;
  total: number | null;
}): number | null {
  if (p.total == null || p.total === 0) return null;
  return Math.min(1, Math.max(0, p.scanned / p.total));
}

/** Whole-percent form of `scanProgressFraction`, for the meter's label. */
export function scanProgressPercent(p: {
  scanned: number;
  total: number | null;
}): number | null {
  const fraction = scanProgressFraction(p);
  return fraction == null ? null : Math.round(fraction * 100);
}

/**
 * Fold one `scan.progress` event into the scanning view's stage list. Stages
 * are identified by phase name only: an event for the phase already in the
 * last row updates that row in place (the service re-publishes the last phase
 * on its heartbeat, so equal consecutive phases are continuation, not a new
 * stage), and an event for a new phase marks the previous row complete - the
 * service sends no per-phase completion event, so completion is inferred
 * from the transition - and appends the new active row at the end. A phase
 * returning after other phases ran in between (metadata extraction,
 * repository scans, and the hashing pass all run inside one analysis loop)
 * starts a new row.
 */
export function appendScanStage(
  rows: ScanStage[],
  event: ScanProgress,
): ScanStage[] {
  const active: ScanStage = { ...event, complete: false };
  const last = rows[rows.length - 1];
  if (last && last.phase === event.phase) {
    return [...rows.slice(0, -1), active];
  }
  if (!last) return [active];
  return [...rows.slice(0, -1), { ...last, complete: true }, active];
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
 * subset of the batch. When `manual` is given, the batch's manual-override
 * count shares the label (`1234 suggestions, 17 manual`) instead of taking a
 * chip of its own.
 */
export function suggestionCountLabel(
  total: number,
  matched: number,
  filtered: boolean,
  manual?: number,
): string {
  const noun = total === 1 ? 'suggestion' : 'suggestions';
  const count = filtered ? `${matched} of ${total} ${noun}` : `${total} ${noun}`;
  return manual === undefined ? count : `${count}, ${manual} manual`;
}
