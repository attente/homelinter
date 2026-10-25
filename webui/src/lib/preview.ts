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

/** Which view the enlarge modal shows. The modal carries the pane's preview
 *  and location choice, so a switch happens in place instead of a dismiss and
 *  a re-open. The requested view wins when its content exists; if it has gone
 *  away (no previewable file, no coordinates) the other view takes over, and
 *  with neither the modal has nothing to show and closes. */
export type EnlargedView = 'preview' | 'location';

export function resolveEnlargedView(
  requested: EnlargedView,
  previewAvailable: boolean,
  mapAvailable: boolean,
): EnlargedView | null {
  const first = requested === 'preview' ? previewAvailable : mapAvailable;
  if (first) return requested;
  const other = requested === 'preview' ? mapAvailable : previewAvailable;
  return other ? (requested === 'preview' ? 'location' : 'preview') : null;
}
