import type { SuggestionStatus, SuggestionType } from '../types';

/** The path whose file a suggestion's preview should show. Once a suggestion
 *  has been executed, its own target no longer exists - the file was moved
 *  away or deleted - so the preview reads from where the file lives now:
 *  a move's destination (`new_path`), or a duplicate delete's retained keeper
 *  (`duplicate_of` metadata). Anything still pending, failed, or skipped
 *  previews its own target, and a post-execution path that is missing falls
 *  back to the target (the service's endpoint resolves the same candidates,
 *  so the URL itself never changes). */
export function previewSourcePath(s: {
  type: SuggestionType;
  status: SuggestionStatus;
  target: string;
  new_path?: string | null;
  metadata?: Record<string, unknown> | null;
}): string {
  if (s.status !== 'executed' && s.status !== 'partial') return s.target;
  if (s.type === 'move') {
    return s.new_path && s.new_path.length > 0 ? s.new_path : s.target;
  }
  const keeper = s.metadata?.['duplicate_of'];
  return typeof keeper === 'string' && keeper.length > 0 ? keeper : s.target;
}
