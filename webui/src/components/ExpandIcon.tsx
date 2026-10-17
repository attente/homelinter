/** Enlarge glyph: four corner brackets pointing outward, used to open a
 *  full-screen preview. */
export function ExpandIcon() {
  return (
    <svg
      viewBox="0 0 16 16"
      width="14"
      height="14"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M6 2 H2 V6 M10 2 H14 V6 M14 10 V14 H10 M6 14 H2 V10"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
