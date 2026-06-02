import { useEffect, useRef, useState } from "react";
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";
import remarkRehype from "remark-rehype";
import rehypeStringify from "rehype-stringify";
import rehypeHighlight from "rehype-highlight";
import { open as openShell } from "@tauri-apps/plugin-shell";

type Props = {
  source: string;
  onWikiLinkClick?: (target: string) => void;
};

const wikiLinkRe = /\[\[([^[\]|]+?)(?:\|([^\]]*))?\]\]/g;
const WIKI_TYPE_PREFIXES = ["entities/", "concepts/", "sources/", "topics/"];

function preprocessWikiLinks(md: string): string {
  return md.replace(wikiLinkRe, (_match, target: string, alias?: string) => {
    const label = alias ?? target;
    return `[${label}](wikilink://${encodeURIComponent(target)})`;
  });
}

/// Pulls a wiki page-id out of a raw markdown link href, or returns null
/// when the href is something else (external URL, anchor, file://, …).
///
/// Mirrors the backend's `page_id_from_markdown_target` so the frontend
/// and the indexer agree on what counts as an internal page reference.
/// Handles three input forms:
///
///   - `concepts/dark-factory`                          → `concepts/dark-factory`
///   - `concepts/dark-factory.md`                       → `concepts/dark-factory`
///   - `http://tauri.localhost/concepts/dark-factory`   → `concepts/dark-factory`
///
/// The third case is what the Tauri webview produces when you read the
/// `href` of a relative link — `getAttribute('href')` keeps it relative,
/// but a click resolves it to that absolute URL, and pages copy-pasted
/// from the address bar can also smuggle that form back into bodies.
function pageIdFromHref(href: string): string | null {
  if (!href) return null;
  if (href.startsWith("#") || href.startsWith("/")) return null;
  if (
    href.startsWith("mailto:") ||
    href.startsWith("file:") ||
    href.startsWith("javascript:")
  )
    return null;

  let stripped = href;
  // Defensive strip of the webview's resolved-absolute prefix.
  for (const prefix of [
    "http://tauri.localhost/",
    "https://tauri.localhost/",
  ]) {
    if (stripped.startsWith(prefix)) {
      stripped = stripped.slice(prefix.length);
      break;
    }
  }
  // A real http(s) link to anywhere else is not a page id.
  if (stripped.startsWith("http://") || stripped.startsWith("https://"))
    return null;

  // Drop query/fragment.
  stripped = stripped.split(/[?#]/)[0];
  // Drop optional `.md`.
  if (stripped.endsWith(".md")) stripped = stripped.slice(0, -3);

  // Must look like `<known-type>/...`.
  if (!WIKI_TYPE_PREFIXES.some((p) => stripped.startsWith(p))) return null;
  return stripped;
}

/// Renders Markdown into a div, intercepting *every* `<a>` click so the
/// Tauri webview can never navigate itself away from the BRAIN UI.
///
///  1. `wikilink://<id>` (preprocessed from `[[id]]`)        → in-app routing
///  2. Markdown links to a wiki-shaped destination            → in-app routing
///     (e.g. `[Dark Factory](concepts/dark-factory)` or even the resolved
///     `http://tauri.localhost/concepts/dark-factory` form)
///  3. `http(s)://`, `mailto:`                                → OS shell
///  4. `#anchor`                                              → default scroll
///  5. Anything else                                          → blocked + warned
///
/// (1) and (2) cover both the canonical wiki-link syntax and the standard
/// markdown link syntax LLM agents tend to emit by default. Without (2)
/// the webview navigated to an unmatched route and the user saw the
/// "Unexpected Application Error" overlay.
/// Search-URL template used by the right-click "Search in browser"
/// action. Google was picked for ubiquity (every default browser
/// knows how to open it). Open issue if Pascal wants this
/// user-configurable via Settings.
const SEARCH_URL = (q: string) =>
  `https://www.google.com/search?q=${encodeURIComponent(q)}`;

type ContextMenuState = {
  x: number;
  y: number;
  selection: string;
};

export function MarkdownRenderer({ source, onWikiLinkClick }: Props) {
  const [html, setHtml] = useState<string>("");
  // Custom right-click menu state. Lives in MarkdownRenderer
  // (rather than a generic global hook) because the only place
  // BRAIN currently shows readable prose is inside this component,
  // and the menu's actions ("Search in browser", "Copy") only make
  // sense when the user has selected text from the rendered body.
  // null = no menu visible.
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  // Ref on the menu so the click-outside listener can ignore clicks
  // that landed on the menu itself (otherwise picking a menu item
  // would close the menu BEFORE the item's onClick fired).
  const menuRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const pre = preprocessWikiLinks(source);
    unified()
      .use(remarkParse)
      .use(remarkGfm)
      .use(remarkRehype)
      .use(rehypeHighlight)
      .use(rehypeStringify)
      .process(pre)
      .then((file) => setHtml(String(file)))
      .catch((err) => setHtml(`<pre>${String(err)}</pre>`));
  }, [source]);

  // Dismiss the context menu on outside click or Esc. Adding
  // listeners only while the menu is open keeps the listener
  // surface small and avoids reacting to clicks in unrelated UI.
  useEffect(() => {
    if (!contextMenu) return;
    function onClick(e: MouseEvent) {
      // Ignore clicks that land on the menu itself — those are
      // menu-item activations and they own the close themselves.
      if (menuRef.current && menuRef.current.contains(e.target as Node)) return;
      setContextMenu(null);
    }
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") setContextMenu(null);
    }
    window.addEventListener("mousedown", onClick);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onClick);
      window.removeEventListener("keydown", onKey);
    };
  }, [contextMenu]);

  function handleContextMenu(e: React.MouseEvent<HTMLDivElement>) {
    const sel = window.getSelection()?.toString().trim() ?? "";
    // No selection → let the webview show its native context menu
    // (or nothing — Tauri 2 has no native context menu by default,
    // but that's the platform choice, not our concern). We only
    // intercept when the user has something to act on.
    if (!sel) return;
    e.preventDefault();
    // Clamp position so the menu doesn't fall off-screen on a
    // right-edge or bottom-edge click.
    const MENU_W = 200;
    const MENU_H = 88;
    const x = Math.min(e.clientX, window.innerWidth - MENU_W - 8);
    const y = Math.min(e.clientY, window.innerHeight - MENU_H - 8);
    setContextMenu({ x, y, selection: sel });
  }

  function searchInBrowser() {
    if (!contextMenu) return;
    const url = SEARCH_URL(contextMenu.selection);
    void openShell(url).catch((err) => {
      console.warn("[BRAIN] failed to open search in browser", err);
    });
    setContextMenu(null);
  }

  function copySelection() {
    if (!contextMenu) return;
    void navigator.clipboard.writeText(contextMenu.selection).catch((err) => {
      console.warn("[BRAIN] clipboard write failed", err);
    });
    setContextMenu(null);
  }

  function handleClick(e: React.MouseEvent<HTMLDivElement>) {
    const anchor = (e.target as HTMLElement | null)?.closest("a");
    if (!anchor) return;
    const href = anchor.getAttribute("href") ?? "";

    // 1. Canonical wiki-link form.
    if (href.startsWith("wikilink://")) {
      e.preventDefault();
      const id = decodeURIComponent(href.slice("wikilink://".length));
      onWikiLinkClick?.(id);
      return;
    }

    // 3. External web/email.
    if (
      href.startsWith("http://") &&
      !href.startsWith("http://tauri.localhost/")
    ) {
      e.preventDefault();
      void openShell(href).catch((err) => {
        console.warn("could not open external link", href, err);
      });
      return;
    }
    if (
      (href.startsWith("https://") &&
        !href.startsWith("https://tauri.localhost/")) ||
      href.startsWith("mailto:")
    ) {
      e.preventDefault();
      void openShell(href).catch((err) => {
        console.warn("could not open external link", href, err);
      });
      return;
    }

    // 4. In-page anchor — let the browser handle it.
    if (href.startsWith("#")) return;

    // 2. Markdown link that *looks like* a wiki page id (relative or
    //    accidentally-absolute via tauri.localhost).
    const pageId = pageIdFromHref(href);
    if (pageId) {
      e.preventDefault();
      onWikiLinkClick?.(pageId);
      return;
    }

    // 5. Anything else: block. The viewer would otherwise navigate
    //    itself out of BRAIN.
    e.preventDefault();
    console.warn(
      `[BRAIN] blocked navigation to non-handled link href: "${href}". ` +
        "Use [[wiki-links]] for internal pages or full URLs for external.",
    );
  }

  return (
    <>
      <div
        className="prose-brain px-6 py-5"
        onClick={handleClick}
        onContextMenu={handleContextMenu}
        dangerouslySetInnerHTML={{ __html: html }}
      />
      {contextMenu && (
        <div
          ref={menuRef}
          role="menu"
          aria-label="Selection actions"
          className="fixed z-50 min-w-[200px] rounded-md border border-neutral-700 bg-neutral-900 py-1 text-sm shadow-2xl"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          <button
            type="button"
            role="menuitem"
            onClick={searchInBrowser}
            className="block w-full px-3 py-1.5 text-left text-neutral-100 hover:bg-neutral-800"
            // Stop propagation so the window-level mousedown
            // listener doesn't see this click and prematurely close
            // the menu before the action runs.
            onMouseDown={(e) => e.stopPropagation()}
          >
            Search in browser
          </button>
          <button
            type="button"
            role="menuitem"
            onClick={copySelection}
            className="block w-full px-3 py-1.5 text-left text-neutral-100 hover:bg-neutral-800"
            onMouseDown={(e) => e.stopPropagation()}
          >
            Copy
          </button>
        </div>
      )}
    </>
  );
}
