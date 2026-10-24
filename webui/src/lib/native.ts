// Native directory picker for the desktop shell. The Tauri WebView serves
// this SPA from the service's loopback origin, so the shell exposes its IPC
// intrinsics (`window.__TAURI_INTERNALS__`) to the page; they only exist
// inside the shell, never in a plain browser.

/** The subset of Tauri's injected intrinsics this module uses. */
interface NativeInternals {
  invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
}

/** The injected object, or null when this is not the desktop shell. */
function tauriInternals(): NativeInternals | null {
  const value = (window as unknown as { __TAURI_INTERNALS__?: unknown })
    .__TAURI_INTERNALS__;
  if (typeof value !== "object" || value === null) return null;
  const candidate = value as Partial<NativeInternals>;
  return typeof candidate.invoke === "function"
    ? (value as NativeInternals)
    : null;
}

/** Whether a native directory picker can be opened from this page. */
export function hasNativeDirectoryPicker(): boolean {
  return tauriInternals() !== null;
}

/**
 * Open the shell's native directory chooser and resolve the absolute path
 * the user picked, or null when they cancelled. The dialog plugin command
 * is enabled for the loopback SPA through the shell's `main` capability.
 * Throws when the picker is unavailable or the shell call fails.
 */
export async function pickDirectory(title: string): Promise<string | null> {
  const native = tauriInternals();
  if (native === null) throw new Error("native directory picker unavailable");
  const result = await native.invoke("plugin:dialog|open", {
    options: { directory: true, multiple: false, title },
  });
  // The dialog plugin resolves the picked folder as a path string, or null
  // when the dialog is cancelled; multi-selection is not requested but the
  // response may still carry an array.
  if (typeof result === "string") return result;
  if (Array.isArray(result) && typeof result[0] === "string") return result[0];
  return null;
}
