/** Back glyph: a left-pointing arrow, used by the run view's top-left
 *  icon button to return to the suggestion browser. */
export function BackIcon() {
  return (
    <svg
      viewBox="0 0 24 24"
      width="14"
      height="14"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M19 12H5"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M12 19l-7-7 7-7"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
