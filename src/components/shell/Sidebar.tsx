import { NavLink } from "react-router-dom";
import { useState } from "react";

const NAV_ITEMS = [
  { to: "/viewer", icon: "📖", label: "Browse", end: true, hint: "Ctrl+1" },
  { to: "/viewer/tier2", icon: "🔍", label: "Search", end: false, hint: "Ctrl+2" },
  { to: "/viewer/graph", icon: "🕸", label: "Graph", end: false, hint: "Ctrl+3" },
  { to: "/wiki-history", icon: "🕓", label: "History", end: false, hint: "Ctrl+H" },
  { to: "/settings", icon: "⚙", label: "Settings", end: false, hint: "Ctrl+," },
];

export function Sidebar() {
  const [collapsed, setCollapsed] = useState(false);
  const width = collapsed ? "w-12" : "w-44";
  return (
    <aside
      className={`flex shrink-0 flex-col border-r border-neutral-800 bg-neutral-950 transition-[width] ${width}`}
    >
      <nav className="flex flex-1 flex-col gap-1 p-2">
        {NAV_ITEMS.map((item) => (
          <NavLink
            key={item.to}
            to={item.to}
            end={item.end}
            className={({ isActive }) =>
              `flex items-center gap-3 rounded-md px-2.5 py-2 text-sm transition-colors ${
                isActive
                  ? "bg-neutral-800 text-emerald-300"
                  : "text-neutral-400 hover:bg-neutral-900 hover:text-neutral-100"
              }`
            }
            title={collapsed ? `${item.label} (${item.hint})` : item.hint}
          >
            <span aria-hidden className="w-5 shrink-0 text-center">
              {item.icon}
            </span>
            {!collapsed && <span className="truncate">{item.label}</span>}
          </NavLink>
        ))}
      </nav>
      <button
        type="button"
        onClick={() => setCollapsed((c) => !c)}
        className="border-t border-neutral-800 px-3 py-2 text-xs text-neutral-500 hover:bg-neutral-900 hover:text-neutral-200"
        aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"}
      >
        {collapsed ? "→" : "← Collapse"}
      </button>
    </aside>
  );
}
