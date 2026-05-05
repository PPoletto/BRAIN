import { useNavigate } from "react-router-dom";
import { useGlobalShortcuts } from "./useGlobalShortcuts";

/// Default Brain-wide shortcuts. Mount once near the app root inside the
/// Router so `useNavigate` works.
///
/// All entries set `globalEvenInInput` so navigation works even with the
/// search bar focused — otherwise typing in the global search would lock
/// the user out of every keyboard navigation. Plain-character shortcuts
/// (without a modifier) intentionally still bail out in inputs to avoid
/// hijacking actual typing.
export function DefaultShortcuts() {
  const navigate = useNavigate();
  useGlobalShortcuts([
    {
      combo: "mod+,",
      description: "Settings",
      action: () => navigate("/settings"),
      globalEvenInInput: true,
    },
    {
      combo: "mod+1",
      description: "Browse (Tier 1)",
      action: () => navigate("/viewer"),
      globalEvenInInput: true,
    },
    {
      combo: "mod+2",
      description: "Search (Tier 2)",
      action: () => navigate("/viewer/tier2"),
      globalEvenInInput: true,
    },
    {
      combo: "mod+3",
      description: "Graph (Tier 3)",
      action: () => navigate("/viewer/graph"),
      globalEvenInInput: true,
    },
    {
      combo: "mod+h",
      description: "Wiki history",
      action: () => navigate("/wiki-history"),
      globalEvenInInput: true,
    },
    {
      combo: "mod+k",
      description: "Quick search",
      action: () => {
        const el = document.querySelector<HTMLInputElement>("[data-global-search]");
        el?.focus();
        el?.select();
      },
      globalEvenInInput: true,
    },
  ]);
  return null;
}
