import type { Toast } from '../lib/toasts';
import { CloseIcon } from './CloseIcon';

interface ToastsProps {
  toasts: Toast[];
  /** Dismiss the toast by its stable key (see `lib/toasts`). */
  onDismiss: (key: string) => void;
}

export function Toasts(props: ToastsProps) {
  const { toasts, onDismiss } = props;
  if (toasts.length === 0) return null;
  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.key} className="toast">
          <span>{t.msg}</span>
          <button
            type="button"
            className="close-btn"
            aria-label="dismiss"
            title="dismiss"
            onClick={() => onDismiss(t.key)}
          >
            <CloseIcon />
          </button>
        </div>
      ))}
    </div>
  );
}
