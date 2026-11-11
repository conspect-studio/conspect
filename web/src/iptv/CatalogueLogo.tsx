// A channel logo from the server's logo cache (`GET …/logos/{k}`).
//
// The endpoint is authenticated like every API route, and a plain `<img src>`
// cannot send the bearer token, so the logo is fetched with the token and shown
// from an object URL. "No logo" is a normal answer (204, any non-ok status, a
// network error, or an <img> that fails to decode): the slot then shows a
// neutral box with the channel's initials, never a broken-image icon. Logos are
// decorative — the channel name is always printed beside them — so both forms
// are aria-hidden.
import { useEffect, useState } from 'react';
import type { JSX } from 'react';
import { logoUrl } from '../api/catalogue';
import { getStoredToken } from '../api/token';

/** Up to two initials from the first words of `name` (letters/digits only). */
function initialsOf(name: string): string {
  const words = name.split(/[\s\-_.]+/u).filter((w) => /[\p{L}\p{N}]/u.test(w));
  const letters = words
    .slice(0, 2)
    .map((w) => Array.from(w.replace(/^[^\p{L}\p{N}]+/u, ''))[0] ?? '');
  return letters.join('').toUpperCase();
}

export function CatalogueLogo({
  logoKey,
  name,
  className,
}: {
  logoKey: string;
  name: string;
  className?: string;
}): JSX.Element {
  const [url, setUrl] = useState<string | null>(null);
  const [brokenKey, setBrokenKey] = useState<string | null>(null);
  const broken = brokenKey === logoKey;

  useEffect(() => {
    const controller = new AbortController();
    let objectUrl: string | null = null;
    const headers: Record<string, string> = {};
    const token = getStoredToken();
    if (token !== undefined) {
      headers.Authorization = `Bearer ${token}`;
    }
    void (async (): Promise<void> => {
      try {
        const resp = await fetch(logoUrl(logoKey), { headers, signal: controller.signal });
        if (!resp.ok || resp.status !== 200) {
          setBrokenKey(logoKey);
          return;
        }
        objectUrl = URL.createObjectURL(await resp.blob());
        setUrl(objectUrl);
      } catch {
        if (!controller.signal.aborted) {
          setBrokenKey(logoKey);
        }
      }
    })();
    return () => {
      controller.abort();
      if (objectUrl !== null) {
        URL.revokeObjectURL(objectUrl);
      }
      setUrl(null);
    };
  }, [logoKey]);

  if (url !== null && !broken) {
    return (
      <img
        src={url}
        alt=""
        aria-hidden="true"
        className={className}
        onError={(): void => {
          setBrokenKey(logoKey);
        }}
      />
    );
  }
  if (!broken) {
    // Still loading: reserve the space without claiming there is no logo.
    return <span aria-hidden="true" className={className} />;
  }
  return (
    <span
      aria-hidden="true"
      data-testid="logo-placeholder"
      className={`${className ?? ''} flex items-center justify-center rounded bg-muted text-[10px] font-medium text-muted-foreground`}
    >
      {initialsOf(name)}
    </span>
  );
}
