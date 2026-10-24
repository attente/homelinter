import { useEffect, useRef } from "react";

type Close = () => void;

/**
 * Escape dismissal for the SPA's dialogs.
 *
 * Modals register their close action while they are open, forming a stack in
 * open order. One shared document-level keydown listener (installed while the
 * stack is non-empty) turns Escape into a dismiss of the topmost dialog only,
 * so a confirmation stacked over the cache viewer closes first and a second
 * Escape closes the viewer. The dismiss calls the same close callback as the
 * dialog's close button and backdrop click, so behaviour is identical to a
 * pointer dismissal.
 */

const stack: Close[] = [];
let listener: ((event: KeyboardEvent) => void) | null = null;

/**
 * Register a modal's close action while it is open. Returns the unregister
 * function for the modal's cleanup.
 */
export function registerModalClose(close: Close): () => void {
  stack.push(close);
  if (stack.length === 1) installListener();
  return () => {
    const index = stack.indexOf(close);
    if (index !== -1) stack.splice(index, 1);
    if (stack.length === 0) removeListener();
  };
}

/**
 * Dismiss the most recently opened modal. The entry is popped before the
 * close callback runs, so a modal that stays mounted for any reason cannot
 * linger in the stack (its later cleanup unregister then finds nothing);
 * the keydown listener uses the return value to leave unhandled Escapes
 * (for example Escape-to-clear in a filter input with no dialog open)
 * untouched.
 */
export function dismissTopmost(): boolean {
  const close = stack.pop();
  if (close === undefined) return false;
  if (stack.length === 0) removeListener();
  close();
  return true;
}

function installListener(): void {
  if (listener !== null || typeof document === "undefined") return;
  listener = (event: KeyboardEvent) => {
    if (event.key !== "Escape") return;
    if (dismissTopmost()) {
      event.preventDefault();
      event.stopPropagation();
    }
  };
  document.addEventListener("keydown", listener, true);
}

function removeListener(): void {
  if (listener === null) return;
  document.removeEventListener("keydown", listener, true);
  listener = null;
}

/**
 * Keep a modal dismissable with Escape while `active` is true. The close
 * callback is read through a ref, so callers can pass an inline arrow
 * without re-registering on every render, and stacked dialogs dismiss one
 * at a time because registration order matches open order.
 */
export function useEscapeClose(active: boolean, close: Close): void {
  const latest = useRef(close);
  useEffect(() => {
    latest.current = close;
  }, [close]);
  useEffect(() => {
    if (!active) return;
    return registerModalClose(() => latest.current());
  }, [active]);
}
