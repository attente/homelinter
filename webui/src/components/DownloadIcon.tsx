/** Download glyph: a downward arrow into a tray, used by the report's
 *  download icon button. */
export function DownloadIcon() {
  return (
    <svg
      viewBox="0 0 16 16"
      width="14"
      height="14"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M8 1.5 V9.5 M4.5 6 L8 9.5 L11.5 6"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M2.5 11.5 V13 A1 1 0 0 0 3.5 14 H12.5 A1 1 0 0 0 13.5 13 V11.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
