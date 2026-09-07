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

export interface ScanDefaults {
  hidden: boolean;
  target_paths: Record<string, string>;
}

export interface Session {
  state: SessionState;
  scan_root: string | null;
  platform: string;
  config_hash: string | null;
  batch_config_hash: string | null;
  suggestions_total: number;
  selection_count: number;
  failure?: ApiErrorBody['error'] | null;
  scan_defaults?: ScanDefaults;
  [key: string]: unknown;
}

export type SuggestionType = 'move' | 'delete';

export type SuggestionStatus =
  | 'pending'
  | 'rejected'
  | 'executed'
  | 'failed'
  | 'skipped';

export type OperationKind = 'move' | 'delete' | 'repair_symlink';
export type OperationStatus = 'completed' | 'failed' | 'skipped';
export type EntryResult = 'executed' | 'failed' | 'skipped';

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
  partial: boolean;
  scan_root: string;
  platform: string;
}

export interface ScanProgress {
  phase: string;
  scanned: number;
  elapsed_ms: number;
}

export interface ExportResult {
  platform: string;
  count: number;
  text: string;
}


export interface ExecuteRequest {
  ids?: string[];
}

export interface ScanRequest {
  root?: string;
  hidden: boolean;
  target_paths?: Record<string, string>;
  /** Partial per-scan config overlay; merged over the effective config
   *  for this scan only (never persisted). */
  config?: Record<string, unknown>;
}

export type SseEventName =
  | 'session.state'
  | 'scan.progress'
  | 'scan.complete'
  | 'suggestion.batch'
  | 'batch.validated'
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
  detail?: Record<string, unknown> | null;
}