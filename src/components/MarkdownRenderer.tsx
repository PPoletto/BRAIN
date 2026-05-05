import { useEffect, useState } from "react";
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
export function MarkdownRenderer({ source, onWikiLinkClick }: Props) {
  const [html, setHtml] = useState<string>("");

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
    <div
      className="prose-brain px-6 py-5"
      onClick={handleClick}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
