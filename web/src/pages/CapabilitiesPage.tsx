// Capabilities — the read-only build capability + licence surface (ADR-W030).
//
// Renders what `GET /api/v1/system/capabilities` reports: which codec backends
// are available, the compositor acceleration tier, the effective build-profile
// licence (a compliance surface — ADR-0012), and the mandatory NDI attribution.
// All reads are best-effort and never block the engine (invariant #10). Status is
// conveyed by VALUE + label, never colour alone (WCAG 1.4.1). This is the
// honest default-build surface; the richer per-device telemetry (per-codec
// profiles, NVENC sessions, VRAM) is a separate SA-1+ milestone.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import { useSystemCapabilities } from '../api/systemQueries';
import type {
  BackendCapability,
  CompositorCapability,
  BuildInfo,
} from '../api/systemQueries';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '../components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';

/** True when an optional value is neither `null` nor `undefined`. */
function present<T>(value: T | null | undefined): value is T {
  return value !== null && value !== undefined;
}

/** Render a backend's probed maximum resolution, or an em dash when unprobed. */
function resolutionText(backend: BackendCapability): string {
  const resolution = backend.max_resolution;
  return present(resolution)
    ? `${String(resolution.width)}×${String(resolution.height)}`
    : '—';
}

/** The per-backend availability matrix (codec decode/encode + software composite). */
function BackendsPanel(props: {
  readonly backends: readonly BackendCapability[];
}): JSX.Element {
  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Backends</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Codec and compositor backends this build can use. A backend is
            available when it is compiled in and a usable device is present.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>
                <Trans>Backend</Trans>
              </TableHead>
              <TableHead>
                <Trans>Stage</Trans>
              </TableHead>
              <TableHead>
                <Trans>Availability</Trans>
              </TableHead>
              <TableHead>
                <Trans>Max resolution</Trans>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {props.backends.map((backend) => (
              <TableRow key={`${backend.kind}-${backend.stage}`}>
                <TableCell className="font-medium">{backend.kind}</TableCell>
                <TableCell>{backend.stage}</TableCell>
                <TableCell>
                  <Badge variant={backend.available ? 'default' : 'outline'}>
                    {backend.available ? (
                      <Trans>Available</Trans>
                    ) : (
                      <Trans>Not available</Trans>
                    )}
                  </Badge>
                </TableCell>
                <TableCell className="tabular-nums">
                  {resolutionText(backend)}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}

/** The compositor acceleration tier (SA-0 classification of the resolved adapter). */
function CompositorPanel(props: {
  readonly compositor: CompositorCapability;
}): JSX.Element {
  const { compositor } = props;
  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Compositor</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            The GPU acceleration tier the compositor resolved on this host. A
            class of “none” means the CPU composite path only.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex items-center justify-between gap-2">
          <span className="text-muted-foreground">
            <Trans>Class</Trans>
          </span>
          <Badge variant="secondary">{compositor.class}</Badge>
        </div>
        {present(compositor.device_type) && (
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">
              <Trans>Device</Trans>
            </span>
            <span className="font-medium">{compositor.device_type}</span>
          </div>
        )}
        {present(compositor.driver) && (
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">
              <Trans>Driver</Trans>
            </span>
            <span className="font-medium">{compositor.driver}</span>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/** The build-profile compliance surface: licence, redistributability, features. */
function BuildPanel(props: {
  readonly build: BuildInfo;
  readonly ndiAttribution:
    | { readonly trademark: string; readonly url: string }
    | undefined;
}): JSX.Element {
  const { build, ndiAttribution } = props;
  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Build &amp; licence</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            The effective build-profile licence of this artifact (its
            codec-linking licence), distinct from the project’s source-available
            licence.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex items-center justify-between gap-2">
          <span className="text-muted-foreground">
            <Trans>Effective licence</Trans>
          </span>
          <Badge variant="secondary">{build.effective_license}</Badge>
        </div>
        <div className="flex items-center justify-between gap-2">
          <span className="text-muted-foreground">
            <Trans>Redistributable</Trans>
          </span>
          <span className="font-medium">
            {build.redistributable ? <Trans>Yes</Trans> : <Trans>No</Trans>}
          </span>
        </div>
        <div>
          <div className="mb-1 text-muted-foreground">
            <Trans>Compiled features</Trans>
          </div>
          <div className="flex flex-wrap gap-1">
            {build.features.length === 0 ? (
              <span className="text-muted-foreground">—</span>
            ) : (
              build.features.map((feature) => (
                <Badge key={feature} variant="outline">
                  {feature}
                </Badge>
              ))
            )}
          </div>
        </div>
        {ndiAttribution !== undefined && (
          <div className="rounded-md border p-3 text-sm">
            <div className="font-medium">{ndiAttribution.trademark}</div>
            <a
              className="text-muted-foreground underline"
              href={ndiAttribution.url}
              target="_blank"
              rel="noreferrer"
            >
              {ndiAttribution.url}
            </a>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/** The Capabilities screen. */
export function CapabilitiesPage(): JSX.Element {
  const query = useSystemCapabilities();

  return (
    <div className="space-y-6">
      <PageHeader
        title={<Trans>Capabilities</Trans>}
        description={
          <Trans>
            What this build can do: available backends, the compositor tier, and
            the effective licence.
          </Trans>
        }
      />
      {query.isPending ? (
        <p className="text-muted-foreground" role="status">
          <Trans>Loading capabilities…</Trans>
        </p>
      ) : query.isError ? (
        <p className="text-destructive" role="alert">
          <Trans>Could not load capabilities.</Trans> {query.error.message}
        </p>
      ) : (
        <div className="grid gap-6 lg:grid-cols-2">
          <div className="lg:col-span-2">
            <BackendsPanel backends={query.data.backends} />
          </div>
          <CompositorPanel compositor={query.data.compositor} />
          <BuildPanel
            build={query.data.build}
            ndiAttribution={query.data.ndi_attribution ?? undefined}
          />
        </div>
      )}
    </div>
  );
}
