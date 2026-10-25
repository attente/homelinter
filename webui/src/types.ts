// TypeScript mirrors of the Service API JSON shapes (see DESIGN.md
// "Service API > Data Types", and crates/model/src/suggestion.rs).

export type SessionState =
  | 'idle'
  | 'scanning'
  | 'analyzing'
  | 'ready'
  | 'executing'
  | 'done'
  | 'failed';

export interface ApiErrorBody {
  error: {
    code: string;
    message: string;
    details?: Record<string, unknown>;
    [key: string]: unknown;
  };
}

export type Theme = 'dark' | 'light';

export interface Session {
  state: SessionState;
  scan_root: string | null;
  home: string;
  platform: string;
  config_hash: string | null;
  batch_config_hash: string | null;
  /** SPA color theme from the effective `[ui].theme`, dark by default. */
  theme: Theme;
  suggestions_total: number;
  selection_count: number;
  /** Move suggestions whose destination metadata currently includes at
   * least one saved manual override. */
  manual_overrides_total: number;
  metadata_progress?: MetadataProgress;
  /** Provider cache lookups for the current batch: the requests answered
   * from the persistent cache, and the ones that had to reach the provider.
   * Reset when a scan plans its provider work; a forced re-request reads
   * neither the cache nor these counters. */
  cache_hits?: number;
  cache_misses?: number;
  /** Scan warnings for the current batch, for example a duplicate group
   * left for manual review; empty until a scan publishes them. */
  warnings?: string[];
  failure?: ApiErrorBody['error'] | null;
  [key: string]: unknown;
}

export type SuggestionType = 'move' | 'delete';

/** Destination metadata fields that a manual override can back. */
export type MetadataField = 'location' | 'subject' | 'author';

export type SuggestionStatus =
  | 'pending'
  | 'rejected'
  | 'executed'
  | 'partial'
  | 'failed'
  | 'skipped';

export type OperationKind = 'move' | 'delete' | 'repair_symlink';
export type OperationStatus = 'completed' | 'failed' | 'skipped';
export type EntryResult = 'executed' | 'partial' | 'failed' | 'skipped';

export interface DependentOperation {
  type: string;
  [key: string]: unknown;
}

export interface Suggestion {
  id: string;
  type: SuggestionType;
  asset_kind: string;
  target: string;
  new_path?: string | null;
  reason: string;
  metadata?: Record<string, unknown>;
  /** Fields currently backed by saved manual overrides rather than provider
   * or embedded values. Absent when the suggestion has none. */
  manual_overrides?: MetadataField[];
  dependent_operations?: DependentOperation[];
  selected: boolean;
  status: SuggestionStatus;
  rejection_reason?: string | null;
  /** True while provider metadata (geocoder/description) is still in
   * flight; such suggestions are visible but not executable. */
  metadata_pending?: boolean;
  [key: string]: unknown;
}

export interface SuggestionList {
  items: Suggestion[];
  total: number;
}

export interface RejectedId {
  id: string;
  reason: string;
}

export interface ValidateResult {
  valid: boolean;
  rejected: RejectedId[];
}

export interface ScanSummary {
  total_files_scanned: number;
  total_symlinks_indexed: number;
  suggestions_by_type: Record<string, number>;
  suggestions_by_kind: Record<string, number>;
  estimated_space_reclaimed: number;
}

export interface OperationOutcome {
  operation: OperationKind;
  source?: string | null;
  destination?: string | null;
  status: OperationStatus;
  detail?: string | null;
}

export interface ReportEntry {
  suggestion_id: string;
  result: EntryResult;
  operations?: OperationOutcome[];
}

export interface ExecutionReport {
  entries: ReportEntry[];
  /** File bytes removed by completed duplicate deletes, not the scan-time
   *  estimate. A partial unit whose delete completed still counts. */
  reclaimed_bytes: number;
  partial: boolean;
  scan_root: string;
  platform: string;
  /** RFC 3339 timestamp of when the scan that produced this batch started. */
  scan_started_at?: string | null;
  /** RFC 3339 timestamp of when execution finished. */
  executed_at?: string | null;
}

export interface ScanProgress {
  phase: string;
  scanned: number;
  /** Work items of this phase when the server knows the total up front;
   * null for phases that cannot know it ahead of time. */
  total: number | null;
  /** Name of the worker currently filling the phase (for example an
   * extractor behind `metadata`), when the phase has several. */
  detail: string | null;
  elapsed_ms: number;
}

/** One persisted row in the scanning view's stage list: a `scan.progress`
 * phase with its last reported counts. A row is marked complete when a later
 * phase starts, because the service sends no per-phase completion event. */
export interface ScanStage extends ScanProgress {
  complete: boolean;
}

/** Progress of outstanding provider metadata requests: `completed`
 * suggestions have had every request resolve, over `total` suggestions
 * that originally had any. `total` is 0 when the batch has none. */
export interface MetadataProgress {
  completed: number;
  total: number;
}

export interface ExportResult {
  platform: string;
  count: number;
  text: string;
}

/** One cached provider result from `GET /cache`. */
export interface CacheEntry {
  cache_key: string;
  kind: string;
  provider: string;
  input_hash: string;
  value: string;
  created_at: string;
}

/** Response body of `GET /cache`. */
export interface CacheList {
  total: number;
  offset: number;
  limit: number;
  kind_counts: { kind: string; count: number }[];
  entries: CacheEntry[];
}


export interface ExecuteRequest {
  ids?: string[];
}

export type SseEventName =
  | 'session.state'
  | 'scan.progress'
  | 'scan.complete'
  | 'suggestion.batch'
  | 'suggestion.updated'
  | 'suggestion.removed'
  | 'batch.validated'
  | 'metadata.progress'
  | 'execution.op'
  | 'execution.complete'
  | 'config.updated'
  | 'cache.cleared';

export interface SseEvent {
  name: SseEventName | string;
  data: unknown;
}

export interface ExecutionOp {
  suggestion_id: string;
  operation: string;
  status: string;
  /** Paths and note of the operation, as published by the service:
   *  `{ source, destination, detail }`. */
  detail?: {
    source?: string | null;
    destination?: string | null;
    detail?: string | null;
  } | null;
}