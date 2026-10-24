/**
 * Clipboard helper shared by the paths in the detail preview and the run
 * list.
 *
 * The async Clipboard API needs a secure context, which the localhost origin
 * normally is; the hidden-textarea `execCommand` path keeps copying working
 * where it is not (an embedded WebView, a non-secure origin). Resolves to
 * whether the copy worked so the caller can confirm it.
 */
export async function copyText(value: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(value);
      return true;
    }
  } catch {
    // Fall through to the legacy path.
  }
  try {
    const el = document.createElement('textarea');
    el.value = value;
    el.style.position = 'fixed';
    el.style.opacity = '0';
    document.body.appendChild(el);
    el.select();
    const ok = document.execCommand('copy');
    document.body.removeChild(el);
    return ok;
  } catch {
    return false;
  }
}
