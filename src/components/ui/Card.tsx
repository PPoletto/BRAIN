import type { HTMLAttributes, ReactNode } from "react";

type Tone = "default" | "danger" | "warning" | "success";

const TONE_STYLES: Record<Tone, string> = {
  default: "border-neutral-800 bg-neutral-900",
  danger: "border-red-900 bg-red-950/40",
  warning: "border-amber-900 bg-amber-950/40",
  success: "border-emerald-900 bg-emerald-950/40",
};

type Props = HTMLAttributes<HTMLDivElement> & {
  tone?: Tone;
  padded?: boolean;
};

export function Card({
  tone = "default",
  padded = true,
  className = "",
  children,
  ...rest
}: Props) {
  const cls = `rounded-lg border shadow-sm ${TONE_STYLES[tone]} ${padded ? "p-5" : ""} ${className}`;
  return (
    <div className={cls} {...rest}>
      {children}
    </div>
  );
}

export function CardHeader({ children }: { children: ReactNode }) {
  return <div className="mb-3 flex items-start justify-between gap-3">{children}</div>;
}

export function CardTitle({ children }: { children: ReactNode }) {
  return <h3 className="text-base font-semibold text-neutral-100">{children}</h3>;
}

export function CardDescription({ children }: { children: ReactNode }) {
  return <p className="mt-0.5 text-sm text-neutral-400">{children}</p>;
}
