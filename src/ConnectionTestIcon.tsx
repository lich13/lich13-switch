export default function ConnectionTestIcon({ size = 14 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <circle cx="3" cy="12" r="2" />
      <path d="M5 12h3l3-6 3 12 3-6h2" />
      <circle cx="21" cy="12" r="2" />
    </svg>
  );
}
