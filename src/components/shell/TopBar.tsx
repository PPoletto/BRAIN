import { useNavigate } from "react-router-dom";
import { MountStatusPill } from "./MountStatusPill";
import { BrainIcon } from "../ui/BrainIcon";

type Props = {
  /** Forwarded into the global-search input so Cmd-K can focus it. */
  onSearch?: (query: string) => void;
};

export function TopBar({ onSearch }: Props) {
  const navigate = useNavigate();
  return (
    <header className="flex h-12 shrink-0 items-center gap-3 border-b border-neutral-800 bg-neutral-950 px-4">
      <button
        type="button"
        onClick={() => navigate("/viewer")}
        className="flex items-center gap-2 text-sm font-semibold text-neutral-100 hover:text-white"
      >
        <BrainIcon size={20} />
        <span>BRAIN</span>
      </button>
      <div className="ml-2 flex-1">
        <input
          data-global-search
          type="search"
          placeholder="Search… (Ctrl+K)"
          onChange={(e) => onSearch?.(e.target.value)}
          className="h-8 w-full max-w-2xl rounded-md border border-neutral-800 bg-neutral-900 px-3 text-sm text-neutral-200 placeholder:text-neutral-600 focus:border-emerald-700 focus:outline-none"
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              navigate(`/viewer/tier2?q=${encodeURIComponent(e.currentTarget.value)}`);
            }
          }}
        />
      </div>
      <MountStatusPill />
      <button
        type="button"
        onClick={() => navigate("/settings")}
        className="rounded-md p-1.5 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100"
        aria-label="Settings"
        title="Settings (Ctrl+,)"
      >
        ⚙
      </button>
    </header>
  );
}
