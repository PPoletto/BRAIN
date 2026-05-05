/// Inline SVG brain logo, kept in sync with `src-tauri/icons/brain-source.svg`.
/// Useful for pre-AppShell screens (Bootstrap, Onboarding header, the
/// "Looking for updates" splash) where the bundled tray icon isn't yet on
/// screen and a CSS-only spinner felt brand-less.
///
/// Pass `pulse` to make it heartbeat-style as a loading affordance — beats
/// the generic spinning ring at communicating that BRAIN itself is busy.

type Props = {
  size?: number | string;
  className?: string;
  /// Render a slow heartbeat scale animation. The brain glows once per
  /// second instead of spinning so it doesn't read as "Windows hourglass".
  pulse?: boolean;
};

export function BrainIcon({ size = 24, className = "", pulse = false }: Props) {
  const dim = typeof size === "number" ? `${size}px` : size;
  return (
    <svg
      viewBox="0 0 1024 1024"
      width={dim}
      height={dim}
      className={`${className} ${pulse ? "brain-pulse" : ""}`}
      aria-hidden
    >
      <defs>
        <linearGradient id="brainIconGradient" x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#34d399" />
          <stop offset="100%" stopColor="#0d9488" />
        </linearGradient>
      </defs>
      {/* left hemisphere */}
      <path
        d="M 512 192 C 380 192, 280 264, 256 364 C 200 372, 160 416, 160 480 C 160 528, 188 568, 232 588 C 220 628, 240 672, 280 696 C 272 740, 308 776, 352 780 C 372 808, 412 824, 460 820 C 480 840, 504 848, 512 848 L 512 192 Z"
        fill="url(#brainIconGradient)"
        stroke="#0f766e"
        strokeWidth="16"
        strokeLinejoin="round"
      />
      {/* right hemisphere */}
      <path
        d="M 512 192 C 644 192, 744 264, 768 364 C 824 372, 864 416, 864 480 C 864 528, 836 568, 792 588 C 804 628, 784 672, 744 696 C 752 740, 716 776, 672 780 C 652 808, 612 824, 564 820 C 544 840, 520 848, 512 848 L 512 192 Z"
        fill="url(#brainIconGradient)"
        stroke="#0f766e"
        strokeWidth="16"
        strokeLinejoin="round"
      />
      {/* sulci */}
      <g
        stroke="#065f46"
        strokeWidth="12"
        strokeLinecap="round"
        fill="none"
        opacity="0.75"
      >
        <path d="M 320 360 Q 380 380 360 440" />
        <path d="M 280 480 Q 360 500 340 560" />
        <path d="M 360 600 Q 420 620 400 680" />
        <path d="M 704 360 Q 644 380 664 440" />
        <path d="M 744 480 Q 664 500 684 560" />
        <path d="M 664 600 Q 604 620 624 680" />
      </g>
      <line
        x1="512"
        y1="208"
        x2="512"
        y2="836"
        stroke="#0f766e"
        strokeWidth="14"
        strokeLinecap="round"
        opacity="0.85"
      />
    </svg>
  );
}
