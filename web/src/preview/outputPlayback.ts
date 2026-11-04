import type { OutputView } from '../resources/types';

const mountOf = (id: string): string => {
  const mount = Array.from(id, (char) => /^[a-zA-Z0-9._~-]$/.test(char) ? char : '-').join('');
  return mount === '' || /^\.+$/.test(mount) ? 'out' : mount;
};

/** The runtime resolves duplicate mounts in configuration order, not resource-list order. */
export function hlsPlaybackCollisionIds(outputs: readonly OutputView[]): ReadonlySet<string> {
  const hls = outputs.filter((output) => output.kind === 'hls' || output.kind === 'll-hls');
  const bases = hls.map((output) => mountOf(output.id));
  const duplicates = bases.filter((base, index) => bases.indexOf(base) !== index);
  return new Set(hls.filter((output) => duplicates.some((base) => {
    const mount = mountOf(output.id);
    // Runtime-added suffixes are numeric (and can chain); a distinct
    // nonnumeric ID such as "a-b-preview" retains its own stable mount.
    return mount === base || (mount.startsWith(`${base}-`) &&
      mount.slice(base.length + 1).split('-').every((part) => /^[2-9][0-9]*$/.test(part) || /^[1-9][0-9]+$/.test(part)));
  })).map((output) => output.id));
}

/** Only expose unambiguous delivery mounts. Saved output configuration is not runtime state. */
export function hlsPlaybackPaths(outputs: readonly OutputView[]): ReadonlyMap<string, string> {
  const hls = outputs.filter((output) => output.kind === 'hls' || output.kind === 'll-hls');
  const collisions = hlsPlaybackCollisionIds(outputs);
  const paths = new Map<string, string>();
  for (const output of hls) {
    if (collisions.has(output.id)) continue;
    const filename = output.target?.split('/').at(-1);
    if (!filename?.endsWith('.m3u8')) continue;
    paths.set(output.id, `/hls/${encodeURIComponent(mountOf(output.id))}/${encodeURIComponent(filename)}`);
  }
  return paths;
}
