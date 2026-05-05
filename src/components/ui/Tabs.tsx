import { type ReactNode } from "react";

type Tab = {
  id: string;
  label: string;
  icon?: ReactNode;
};

type Props = {
  tabs: Tab[];
  active: string;
  onChange: (id: string) => void;
  className?: string;
};

export function Tabs({ tabs, active, onChange, className = "" }: Props) {
  return (
    <div role="tablist" className={`flex border-b border-neutral-800 ${className}`}>
      {tabs.map((t) => {
        const selected = t.id === active;
        return (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={selected}
            onClick={() => onChange(t.id)}
            className={`flex items-center gap-2 border-b-2 px-4 py-2.5 text-sm transition-colors ${
              selected
                ? "border-emerald-500 text-neutral-100"
                : "border-transparent text-neutral-400 hover:border-neutral-700 hover:text-neutral-200"
            }`}
          >
            {t.icon}
            {t.label}
          </button>
        );
      })}
    </div>
  );
}
