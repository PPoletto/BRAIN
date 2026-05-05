import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

type Variant = "primary" | "secondary" | "destructive" | "ghost";
type Size = "sm" | "md" | "lg";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant;
  size?: Size;
  /** When true, renders a spinner before children and disables the button.
   *  Use together with `useAsyncAction` so async handlers show feedback. */
  loading?: boolean;
};

const VARIANT_STYLES: Record<Variant, string> = {
  primary:
    "bg-emerald-600 text-white hover:bg-emerald-500 disabled:bg-emerald-900 disabled:text-neutral-400",
  secondary:
    "border border-neutral-700 bg-neutral-900 text-neutral-100 hover:bg-neutral-800 disabled:opacity-50",
  destructive:
    "bg-red-700 text-white hover:bg-red-600 disabled:bg-red-950 disabled:text-neutral-400",
  ghost:
    "text-neutral-300 hover:bg-neutral-800 hover:text-neutral-100 disabled:opacity-50",
};

const SIZE_STYLES: Record<Size, string> = {
  sm: "h-7 px-2 text-xs",
  md: "h-9 px-3 text-sm",
  lg: "h-11 px-4 text-sm font-medium",
};

export const Button = forwardRef<HTMLButtonElement, Props>(
  (
    {
      variant = "secondary",
      size = "md",
      className = "",
      loading = false,
      disabled,
      children,
      ...rest
    },
    ref,
  ) => {
    const cls = `inline-flex items-center justify-center gap-1.5 rounded-md transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-emerald-500/60 disabled:cursor-not-allowed ${VARIANT_STYLES[variant]} ${SIZE_STYLES[size]} ${className}`;
    const isDisabled = disabled || loading;
    return (
      <button ref={ref} className={cls} disabled={isDisabled} aria-busy={loading} {...rest}>
        {loading && <Spinner />}
        {children as ReactNode}
      </button>
    );
  },
);
Button.displayName = "Button";

function Spinner() {
  return (
    <svg
      className="size-3.5 animate-spin"
      viewBox="0 0 24 24"
      fill="none"
      aria-hidden
    >
      <circle
        cx="12"
        cy="12"
        r="10"
        stroke="currentColor"
        strokeWidth="3"
        opacity="0.25"
      />
      <path
        d="M4 12a8 8 0 0 1 8-8"
        stroke="currentColor"
        strokeWidth="3"
        strokeLinecap="round"
      />
    </svg>
  );
}
