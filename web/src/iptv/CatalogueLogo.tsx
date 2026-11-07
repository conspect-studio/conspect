// A channel logo from the server's logo cache (`GET …/logos/{k}`).
//
// The endpoint is authenticated like every API route, and a plain `<img src>`
// cannot send the bearer token, so the logo is fetched with the token and shown
// from an object URL. A missing or failed logo renders nothing: logos are
// decorative here (the channel name is always printed beside it).
import { useEffect, useState } from 'react';
import type { JSX } from 'react';
import { logoUrl } from '../api/catalogue';
import { getStoredToken } from '../api/token';

export function CatalogueLogo({ logoKey, className }: { logoKey: string; className?: string }): JSX.Element | null {
  const [url, setUrl] = useState<string | null>(null);

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
        if (!resp.ok) {
          return;
        }
        objectUrl = URL.createObjectURL(await resp.blob());
        setUrl(objectUrl);
      } catch {
        // Decorative: a failed logo leaves the slot empty.
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

  return url === null ? null : <img src={url} alt="" className={className} />;
}
