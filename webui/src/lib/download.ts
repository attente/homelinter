// Blob download helpers for exported plans and the execution report.

/** Minimal shape of the File System Access API's save picker, which the DOM
 *  typings of the pinned TypeScript release do not declare. */
interface SaveFilePickerWindow extends Window {
  showSaveFilePicker?: (options?: {
    suggestedName?: string;
    types?: { description: string; accept: Record<string, string[]> }[];
  }) => Promise<FileSystemFileHandle>;
}

/** Save text with a plain anchor download, which lands in the browser's
 *  configured download location. */
export function downloadText(
  filename: string,
  text: string,
  mime = 'text/x-sh',
): void {
  const blob = new Blob([text], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

/** File extension of `filename`, or `.txt` when it has none. */
function typeExtension(filename: string): string {
  const dot = filename.lastIndexOf('.');
  return dot > 0 ? filename.slice(dot) : '.txt';
}

/**
 * Save text to a file the user chooses. Where the browser supports the File
 * System Access API (Chromium on this localhost origin), `showSaveFilePicker`
 * opens the platform save dialog so the destination is picked explicitly;
 * elsewhere (Firefox, Safari, embedded WebViews) the plain anchor download
 * falls back to the browser's own download handling. Resolves false when the
 * user cancels the save dialog, so the caller can skip a success notice.
 */
export async function saveTextFile(
  filename: string,
  text: string,
  mime = 'text/plain',
  description = 'text file',
): Promise<boolean> {
  const win = window as SaveFilePickerWindow;
  if (!win.showSaveFilePicker) {
    downloadText(filename, text, mime);
    return true;
  }
  let handle: FileSystemFileHandle;
  try {
    // Called on `window` itself: the picker is a Window operation and an
    // unbound call throws an illegal-invocation error in Chromium.
    handle = await win.showSaveFilePicker({
      suggestedName: filename,
      types: [
        { description, accept: { [mime]: [typeExtension(filename)] } },
      ],
    });
  } catch (e: unknown) {
    if ((e as { name?: string } | null)?.name === 'AbortError') return false;
    throw e;
  }
  const writable = await handle.createWritable();
  await writable.write(new Blob([text], { type: mime }));
  await writable.close();
  return true;
}
