interface CloseIconProps {
  className?: string;
}

export function CloseIcon(props: CloseIconProps) {
  const { className } = props;
  return (
    <svg
      className={className}
      viewBox="0 0 16 16"
      width="14"
      height="14"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M4 4 L12 12 M12 4 L4 12"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
      />
    </svg>
  );
}
