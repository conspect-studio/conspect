// The documentation search box (ADR-W016). Queries the registry-derived
// MiniSearch index entirely client-side and renders a keyboard-operable list
// of deep links (page title + matched section). Arrow keys move between the
// input and result links; Escape clears; Enter follows the focused link (or
// the first result when pressed in the input).
import type { JSX, KeyboardEvent } from "react";
import { useId, useMemo, useRef, useState } from "react";
import { useLingui } from "@lingui/react";
import { Trans, useLingui as useLinguiMacro } from "@lingui/react/macro";
import { Search } from "lucide-react";
import { Link, useNavigate } from "react-router-dom";

import { createDocsSearch } from "../../docs/search";
import type { DocsSearchHit } from "../../docs/search";

/** A search box over the in-app documentation. */
export function DocsSearch(): JSX.Element {
  const { i18n } = useLingui();
  const { t } = useLinguiMacro();
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const listId = useId();
  const listRef = useRef<HTMLUListElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  // Rebuild the index only when the active locale changes; the registry
  // itself is static.
  const search = useMemo(
    () => createDocsSearch((descriptor) => i18n._(descriptor)),
    [i18n],
  );
  const hits = useMemo(() => search(query), [search, query]);
  const trimmed = query.trim();

  function hitHref(hit: DocsSearchHit): string {
    return hit.sectionId === null ? hit.path : `${hit.path}#${hit.sectionId}`;
  }

  function focusResult(index: number): void {
    const links = listRef.current?.querySelectorAll("a");
    const link = links?.item(index);
    if (link instanceof HTMLAnchorElement) {
      link.focus();
    }
  }

  function onInputKeyDown(event: KeyboardEvent<HTMLInputElement>): void {
    if (event.key === "ArrowDown" && hits.length > 0) {
      event.preventDefault();
      focusResult(0);
    } else if (event.key === "Enter" && hits.length > 0) {
      const first = hits[0];
      if (first !== undefined) {
        event.preventDefault();
        setQuery("");
        void navigate(hitHref(first));
      }
    } else if (event.key === "Escape") {
      setQuery("");
    }
  }

  function onResultKeyDown(
    event: KeyboardEvent<HTMLAnchorElement>,
    index: number,
  ): void {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      focusResult(Math.min(index + 1, hits.length - 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      if (index === 0) {
        inputRef.current?.focus();
      } else {
        focusResult(index - 1);
      }
    } else if (event.key === "Escape") {
      setQuery("");
      inputRef.current?.focus();
    }
  }

  return (
    <div className="space-y-2">
      <div className="relative">
        <Search
          className="pointer-events-none absolute start-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground"
          aria-hidden="true"
        />
        <input
          ref={inputRef}
          type="search"
          value={query}
          onChange={(event) => {
            setQuery(event.target.value);
          }}
          onKeyDown={onInputKeyDown}
          aria-label={t`Search documentation`}
          placeholder={t`Search docs…`}
          aria-controls={listId}
          autoComplete="off"
          className="h-9 w-full rounded-md border bg-background ps-8 pe-3 text-sm text-foreground placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        />
      </div>
      <p aria-live="polite" className="sr-only">
        {trimmed.length === 0 ? (
          ""
        ) : (
          <Trans>{hits.length} documentation results</Trans>
        )}
      </p>
      {trimmed.length > 0 ? (
        hits.length > 0 ? (
          <ul
            id={listId}
            ref={listRef}
            aria-label={t`Search results`}
            className="overflow-hidden rounded-md border bg-card"
          >
            {hits.map((hit, index) => (
              <li
                key={`${hit.path}#${hit.sectionId ?? ""}`}
                className="border-b last:border-b-0"
              >
                <Link
                  to={hitHref(hit)}
                  onClick={() => {
                    setQuery("");
                  }}
                  onKeyDown={(event) => {
                    onResultKeyDown(event, index);
                  }}
                  className="block px-3 py-2 text-sm hover:bg-accent hover:text-accent-foreground focus-visible:bg-accent focus-visible:text-accent-foreground focus-visible:outline-none"
                >
                  <span className="font-medium text-foreground">
                    {hit.pageTitle}
                  </span>
                  {hit.sectionId !== null ? (
                    <span className="block text-muted-foreground">
                      {hit.title}
                    </span>
                  ) : null}
                </Link>
              </li>
            ))}
          </ul>
        ) : (
          <p id={listId} className="px-1 text-sm text-muted-foreground">
            <Trans>No results.</Trans>
          </p>
        )
      ) : null}
    </div>
  );
}
