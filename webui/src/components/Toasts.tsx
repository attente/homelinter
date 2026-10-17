import { CloseIcon } from './CloseIcon';

interface ToastsProps {
  toasts: string[];
  onDismiss: (index: number) => void;
}

export function Toasts(props: ToastsProps) {
  const { toasts, onDismiss } = props;
  if (toasts.length === 0) return null;
  return (
    <div className="toasts">
      {toasts.map((t, i) => (
        <div key={`${i}-${t}`} className="toast">
          <span>{t}</span>
          <button type="button" aria-label="dismiss" title="dismiss" onClick={() => onDismiss(i)}>
            <CloseIcon />
          </button>
        </div>
      ))}
    </div>
  );
}
