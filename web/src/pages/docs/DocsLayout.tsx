// The documentation shell (ADR-W016). Renders the docs search box, a grouped
// table-of-contents nav landmark, a registry-derived breadcrumb, the active
// topic page in an <Outlet>, and a registry-derived "Related" footer. On
// navigation it resolves anchor redirects, scrolls to the URL-fragment
// section, and briefly highlights it (honoring prefers-reduced-motion). Sits
// inside the app's <main>; the topic page owns the route's single <h1>.
import type { JSX } from "react";
import { useEffect } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Link, NavLink, Outlet, useLocation, useNavigate } from "react-router-dom";

import { cn } from "../../lib/utils";
import { getDocsPage, resolveAnchor } from "../../docs/registry";
import { DocsSearch } from "./DocsSearch";
import { DOCS_NAV_CONCEPTS, DOCS_NAV_GUIDES } from "./docsNav";
import type { DocsNavItem } from "./docsNav";

// Tailwind classes applied to the targeted <section> while highlighted.
// Listed literally so the Tailwind scanner emits them.
const HIGHLIGHT_CLASSES = ["ring-2", "ring-ring", "transition-shadow"];
const HIGHLIGHT_MS = 2000;
// The lazy concept chunks mount after the layout's effect on a cold deep
// link; retry briefly until the anchor target exists.
const ANCHOR_RETRY_MS = 100;
const ANCHOR_RETRY_LIMIT = 20;

function NavGroup({
  heading,
  items,
}: {
  readonly heading: JSX.Element;
  readonly items: readonly DocsNavItem[];
}): JSX.Element {
  const { i18n } = useLingui();
  return (
    <>
      <p className="px-3 pb-2 pt-4 text-xs font-semibold uppercase tracking-wide text-muted-foreground first:pt-0">
        {heading}
      </p>
      <ul className="flex flex-col gap-1">
        {items.map(({ path, title, Icon }) => (
          <li key={path}>
            <NavLink
              to={path}
              end={path === "/help"}
              className={({ isActive }): string =>
                cn(
                  "flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium transition-colors hover:bg-accent hover:text-accent-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                  isActive
                    ? "bg-accent text-accent-foreground"
                    : "text-muted-foreground",
                )
              }
            >
              {({ isActive }): JSX.Element => (
                <>
                  <Icon className="size-4 shrink-0" aria-hidden="true" />
                  <span>{i18n._(title)}</span>
                  {isActive ? <span className="sr-only">(current)</span> : null}
                </>
              )}
            </NavLink>
          </li>
        ))}
      </ul>
    </>
  );
}

/** Layout route for the in-app documentation under `/help`. */
export function DocsLayout(): JSX.Element {
  const { t, i18n } = useLingui();
  const location = useLocation();
  const navigate = useNavigate();
  const page = getDocsPage(location.pathname);
  const related = (page?.related ?? []).flatMap((path) => {
    const entry = getDocsPage(path);
    return entry === undefined ? [] : [entry];
  });

  // Resolve anchor redirects, then scroll to and highlight the fragment
  // target whenever the location (including its hash) changes.
  useEffect(() => {
    const id = location.hash.startsWith("#") ? location.hash.slice(1) : "";
    if (id.length === 0) {
      return undefined;
    }
    const target = resolveAnchor(location.pathname, id);
    if (target.path !== location.pathname || target.id !== id) {
      void navigate(`${target.path}#${target.id}`, { replace: true });
      return undefined;
    }
    const reduceMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    let attempts = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let unhighlight: ReturnType<typeof setTimeout> | undefined;
    let element: HTMLElement | null = null;

    const tryScroll = (): void => {
      element = document.getElementById(id);
      if (element === null) {
        attempts += 1;
        if (attempts < ANCHOR_RETRY_LIMIT) {
          timer = setTimeout(tryScroll, ANCHOR_RETRY_MS);
        }
        return;
      }
      element.scrollIntoView({
        behavior: reduceMotion ? "auto" : "smooth",
        block: "start",
      });
      element.classList.add(...HIGHLIGHT_CLASSES);
      const found = element;
      unhighlight = setTimeout(() => {
        found.classList.remove(...HIGHLIGHT_CLASSES);
      }, HIGHLIGHT_MS);
    };
    tryScroll();

    return (): void => {
      if (timer !== undefined) {
        clearTimeout(timer);
      }
      if (unhighlight !== undefined) {
        clearTimeout(unhighlight);
      }
      element?.classList.remove(...HIGHLIGHT_CLASSES);
    };
  }, [location, navigate]);

  return (
    <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
      <div className="shrink-0 lg:sticky lg:top-6 lg:w-60">
        <DocsSearch />
        <nav aria-label={t`Documentation`} className="mt-4">
          <NavGroup
            heading={<Trans>Documentation</Trans>}
            items={DOCS_NAV_GUIDES}
          />
          <NavGroup
            heading={<Trans>Concepts</Trans>}
            items={DOCS_NAV_CONCEPTS}
          />
        </nav>
      </div>

      <div className="min-w-0 flex-1">
        {page !== undefined && page.path !== "/help" ? (
          <nav aria-label={t`Breadcrumb`} className="mb-4">
            <ol className="flex flex-wrap items-center gap-1.5 text-sm text-muted-foreground">
              <li>
                <Link
                  to="/help"
                  className="underline-offset-4 hover:text-foreground hover:underline focus-visible:outline-2 focus-visible:outline-ring"
                >
                  <Trans>Documentation</Trans>
                </Link>
              </li>
              <li aria-hidden="true">›</li>
              <li aria-current="page" className="text-foreground">
                {i18n._(page.title)}
              </li>
            </ol>
          </nav>
        ) : null}

        <Outlet />

        {related.length > 0 ? (
          <footer className="mt-8 border-t pt-6">
            <h2 className="text-sm font-semibold uppercase tracking-wide text-muted-foreground">
              <Trans>Related</Trans>
            </h2>
            <ul className="mt-3 space-y-2">
              {related.map((entry) => (
                <li key={entry.path}>
                  <Link
                    to={entry.path}
                    className="font-medium text-foreground underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    {i18n._(entry.title)}
                  </Link>
                  <span className="block text-sm text-muted-foreground">
                    {i18n._(entry.summary)}
                  </span>
                </li>
              ))}
            </ul>
          </footer>
        ) : null}
      </div>
    </div>
  );
}
