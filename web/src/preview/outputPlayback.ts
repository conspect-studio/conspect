import type { OutputView } from '../resources/types';

/** Only expose unambiguous delivery mounts. Saved output configuration is not runtime state. */
export function hlsPlaybackPaths(outputs: readonly OutputView[]): ReadonlyMap<string, string> {
  const hls = outputs.filter((output) => output.kind === 'hls' || output.kind === 'll-hls');
  const mountOf = (id: string): string => {
    const mount = Array.from(id, (char) => /^[a-zA-Z0-9._~-]$/.test(char) ? char : '-').join('');
    return mount === '' || /^\.+$/.test(mount) ? 'out' : mount;
  };
  const paths = new Map<string, string>();
  for (const output of hls) {
    const mount = mountOf(output.id);
    // The runtime suffixes colliding mounts in config order, which the resource
    // list does not guarantee. Never guess the target of an ambiguous mount.
    const bases = hls.map((candidate) => mountOf(candidate.id));
    const duplicates = bases.filter((base, index) => bases.indexOf(base) !== index);
    if (duplicates.some((base) => mount === base || mount.startsWith(`${base}-`))) continue;
    const filename = output.target?.split('/').at(-1);
    if (!filename?.endsWith('.m3u8')) continue;
    paths.set(output.id, `/hls/${encodeURIComponent(mount)}/${encodeURIComponent(filename)}`);
  }
  return paths;
}
