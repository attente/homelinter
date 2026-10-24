/**
 * Toast list logic for the SPA's transient notices.
 *
 * Notices are identified by a stable key instead of by their text: a notice
 * that repeats while an entry with the same key is still visible updates that
 * entry (and restarts its dismiss clock through the caller) rather than
 * stacking another copy. A burst of duplicate notices, such as one
 * re-validation rejection per rebuilt suggestion, therefore reads as a single
 * toast, and counted notices can rebuild their label from the running total.
 * The list is also capped, so even a storm of distinct notices cannot cover
 * the screen; the oldest entry falls off the front past the cap.
 */

/** One visible notice. */
export interface Toast {
  /** Stable identity: a toast with the same key merges into one entry. */
  key: string;
  /** Text shown to the user. */
  msg: string;
  /** Notices merged into this entry; `addCountedToast` builds its label from
   *  this running total. */
  count: number;
}

/** Visible notices never exceed this many; the oldest is dropped first. */
export const MAX_TOASTS = 4;

/** Replace the entry for `key` in place, or append it and drop the oldest
 *  entry when the list is full. In-place replacement keeps a refreshed toast
 *  where the user already expects it. */
function upsert(
  list: Toast[],
  key: string,
  msg: string,
  count: number,
  max: number,
): Toast[] {
  const index = list.findIndex((t) => t.key === key);
  if (index === -1) {
    return [...list, { key, msg, count }].slice(-max);
  }
  const next = list.slice();
  next[index] = { key, msg, count };
  return next;
}

/**
 * Show `msg`, merging it into a visible toast with the same key. The caller
 * restarts that toast's dismiss clock, so the merged toast stays up while the
 * notices keep arriving.
 */
export function addToast(
  list: Toast[],
  key: string,
  msg: string,
  max = MAX_TOASTS,
): Toast[] {
  const previous = list.find((t) => t.key === key);
  return upsert(list, key, msg, (previous?.count ?? 0) + 1, max);
}

/**
 * Show an aggregated notice: `amount` is added to the running total of the
 * toast with this key and the entry renders `label(total)`, so repeated
 * amounts read as one notice with the sum (`re-validation rejected 3
 * suggestion(s)`) instead of one toast per amount.
 */
export function addCountedToast(
  list: Toast[],
  key: string,
  amount: number,
  label: (total: number) => string,
  max = MAX_TOASTS,
): Toast[] {
  const previous = list.find((t) => t.key === key);
  const count = (previous?.count ?? 0) + amount;
  return upsert(list, key, label(count), count, max);
}

/** Drop the toast with this key; an absent key is a no-op. */
export function removeToast(list: Toast[], key: string): Toast[] {
  return list.filter((t) => t.key !== key);
}
