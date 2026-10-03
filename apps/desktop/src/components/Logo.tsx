export function Logo({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 1024 1024" className={className} aria-hidden>
      <rect width="1024" height="1024" rx="224" fill="var(--bg-raised)" />
      <rect
        x="24"
        y="24"
        width="976"
        height="976"
        rx="204"
        fill="none"
        stroke="var(--border-strong)"
        strokeWidth="8"
      />
      <g fill="none" stroke="var(--text)" strokeWidth="72" strokeLinecap="round">
        <circle cx="352" cy="480" r="150" />
        <path d="M462 590 L540 690" />
        <circle cx="672" cy="480" r="150" />
        <path d="M782 590 L860 690" />
      </g>
      <rect x="240" y="800" width="120" height="40" rx="20" fill="var(--accent)" />
    </svg>
  );
}
