import { useEffect, useRef, useState } from 'react';
import { copyText } from '../lib/clipboard';
import { CheckIcon, CopyIcon } from './CopyIcon';

interface CopyablePathProps {
  /** Full path copied by the path text and the trailing button. */
  value: string;
  /** Visible text when the path is shortened; defaults to `value`. */
  display?: string;
  /** Class for the clickable path text, so the caller keeps its typography. */
  className: string;
  /** Class for the trailing clipboard button; defaults to the preview size. */
  buttonClassName?: string;
}

/**
 * A path whose text copies the full value on click, with a clipboard icon
 * button after it for the same action. The button confirms the copy with a
 * check for a moment. Both share one copied state so either trigger confirms
 * visibly.
 */
export function CopyablePath(props: CopyablePathProps) {
  const { value, display = value, className, buttonClassName } = props;
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );
  const copy = async () => {
    if (await copyText(value)) {
      setCopied(true);
      if (timer.current !== null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1500);
    }
  };
  const buttonLabel = copied ? 'copied' : 'copy path to clipboard';
  return (
    <>
      <span
        className={className}
        role="button"
        tabIndex={0}
        title="copy path to clipboard"
        onClick={() => void copy()}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            void copy();
          }
        }}
      >
        {display}
      </span>
      <button
        type="button"
        className={
          buttonClassName === undefined
            ? 'path-copy-btn'
            : `path-copy-btn ${buttonClassName}`
        }
        title={buttonLabel}
        aria-label={buttonLabel}
        onClick={() => void copy()}
      >
        {copied ? <CheckIcon /> : <CopyIcon />}
      </button>
    </>
  );
}
