// Support (/help/support) — the entitlement-gated support surface (§10, ADR-0053).
//
// ENTITLEMENT-GATED (GET /api/v1/support/entitlement):
//  - eligible → raise-ticket form (severity question/degraded/blocking, the
//    auto-attached machine context shown BEFORE submit, the routing per the
//    entitlement) + the ticket list/thread/reply.
//  - free → community links + one quiet line (no urgency, no upsell theatre).
// CONTEXT-PACK COMPOSER (everyone): pick a window + the sections to include,
// compose a REDACTED bundle (POST → 202 → read the preview), and see the
// redaction list (a location, never the value) + the on-screen statement that the
// pack carries no media and leaves the machine only on explicit action.
import { useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { composeBundle, getBundle } from "../api/conspect";
import type {
  Bundle,
  BundleInclude,
  BundleWindow,
  TicketSeverity,
} from "../api/conspectQueries";
import {
  useLicence,
  useRaiseTicket,
  useReplyToTicket,
  useSupportEntitlement,
  useTicket,
  useTickets,
} from "../api/conspectQueries";
import { PageHeader } from "../components/PageHeader";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import { Input } from "../components/ui/input";
import { Label } from "../components/ui/label";
import { toast } from "../components/ui/use-toast";

const SEVERITIES: readonly TicketSeverity[] = ["question", "degraded", "blocking"];
const WINDOWS: readonly BundleWindow[] = ["1h", "24h", "7d"];
const INCLUDES: readonly BundleInclude[] = ["diagnostics", "metrics", "config", "incidents"];

/** Narrow a raw select value to a known severity, defaulting safely. */
function toSeverity(raw: string): TicketSeverity {
  return SEVERITIES.find((s) => s === raw) ?? "question";
}

/** Narrow a raw select value to a known window, defaulting safely. */
function toWindow(raw: string): BundleWindow {
  return WINDOWS.find((w) => w === raw) ?? "24h";
}

/** The raise-ticket form with auto-attached context shown before submit. */
function RaiseTicketForm({ routeTo }: { readonly routeTo: string }): JSX.Element {
  const { t } = useLingui();
  const [subject, setSubject] = useState("");
  const [body, setBody] = useState("");
  const [severity, setSeverity] = useState<TicketSeverity>("question");
  const raise = useRaiseTicket();
  // The context auto-attached to a ticket is computed server-side; we show the
  // operator-visible parts (tier + enforcement level) from the licence resource
  // before submit so there are no surprises about what is attached.
  const licence = useLicence();
  const status = licence.data?.status ?? null;

  const submit = (): void => {
    raise.mutate(
      { subject, body, severity },
      {
        onSuccess: () => {
          setSubject("");
          setBody("");
          setSeverity("question");
          toast({ title: t`Ticket raised` });
        },
        onError: (error) => {
          toast({
            title: t`Could not raise the ticket`,
            description: error.message,
            variant: "destructive",
          });
        },
      },
    );
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Raise a ticket</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>Raise a support ticket against your entitled queue.</Trans>
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form
          data-testid="raise-ticket-form"
          className="space-y-3"
          onSubmit={(e): void => {
            e.preventDefault();
            submit();
          }}
        >
          <p className="text-sm text-muted-foreground">
            <Trans>Routes to</Trans> <span className="font-mono">{routeTo}</span>
          </p>
          <div className="grid gap-1.5">
            <Label htmlFor="ticket-subject">
              <Trans>Subject</Trans>
            </Label>
            <Input
              id="ticket-subject"
              value={subject}
              onChange={(e): void => {
                setSubject(e.target.value);
              }}
            />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="ticket-body">
              <Trans>Description</Trans>
            </Label>
            <textarea
              id="ticket-body"
              rows={4}
              className="min-h-[88px] rounded-md border border-input bg-background px-3 py-2 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={body}
              onChange={(e): void => {
                setBody(e.target.value);
              }}
            />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="ticket-severity">
              <Trans>Severity</Trans>
            </Label>
            <select
              id="ticket-severity"
              className="h-9 min-h-[44px] w-48 rounded-md border border-input bg-background px-3 text-sm"
              value={severity}
              onChange={(e): void => {
                setSeverity(toSeverity(e.target.value));
              }}
            >
              <option value="question">{t`Question`}</option>
              <option value="degraded">{t`Degraded`}</option>
              <option value="blocking">{t`Blocking`}</option>
            </select>
          </div>

          <div
            data-testid="ticket-context"
            className="rounded-md border bg-muted/40 p-3 text-sm text-muted-foreground"
          >
            <p className="mb-1 font-medium text-foreground">
              <Trans>Attached automatically</Trans>
            </p>
            <p className="mb-2">
              <Trans>
                The machine version, tier, enforcement level, and salted
                fingerprint score — a number, never raw identifiers.
              </Trans>
            </p>
            {status !== null ? (
              <ul className="space-y-0.5">
                <li>
                  <Trans>Tier</Trans> <span className="font-mono">{status.tier}</span>
                </li>
                <li>
                  <Trans>Enforcement</Trans>{" "}
                  <span className="font-mono">{status.enforcement}</span>
                </li>
              </ul>
            ) : null}
          </div>

          <Button type="submit" disabled={raise.isPending || subject === ""}>
            <Trans>Raise ticket</Trans>
          </Button>
        </form>
      </CardContent>
    </Card>
  );
}

/** A single ticket's thread + reply box. */
function TicketThread({ id }: { readonly id: string }): JSX.Element {
  const { t } = useLingui();
  const ticket = useTicket(id);
  const reply = useReplyToTicket();
  const [text, setText] = useState("");

  const send = (): void => {
    reply.mutate(
      { id, body: text },
      {
        onSuccess: () => {
          setText("");
        },
        onError: (error) => {
          toast({
            title: t`Could not send the reply`,
            description: error.message,
            variant: "destructive",
          });
        },
      },
    );
  };

  return (
    <div data-testid="ticket-thread" className="space-y-3">
      {ticket.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading ticket…</Trans>
        </p>
      ) : ticket.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load the ticket:</Trans> {ticket.error.message}
        </p>
      ) : (
        <>
          <ul className="space-y-2">
            {ticket.data.updates.map((update, i) => (
              <li
                key={`${String(update.at_nanos)}-${String(i)}`}
                className="rounded-md border px-3 py-2 text-sm"
              >
                <p className="mb-1 font-mono text-xs text-muted-foreground">
                  {update.author}
                </p>
                <p>{update.body}</p>
              </li>
            ))}
          </ul>

          {ticket.data.state === "open" ? (
            <form
              className="space-y-2"
              onSubmit={(e): void => {
                e.preventDefault();
                send();
              }}
            >
              <Label htmlFor="ticket-reply">
                <Trans>Reply</Trans>
              </Label>
              <textarea
                id="ticket-reply"
                rows={3}
                className="block min-h-[72px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                value={text}
                onChange={(e): void => {
                  setText(e.target.value);
                }}
              />
              <Button type="submit" disabled={reply.isPending || text === ""}>
                <Trans>Send reply</Trans>
              </Button>
            </form>
          ) : (
            <p className="text-sm text-muted-foreground">
              <Trans>This ticket is closed.</Trans>
            </p>
          )}
        </>
      )}
    </div>
  );
}

/** The ticket list + the selected thread. */
function TicketsPanel(): JSX.Element {
  const tickets = useTickets();
  const [selected, setSelected] = useState<string | undefined>(undefined);
  const rows = tickets.data ?? [];

  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Your tickets</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>Open a ticket to read and reply to its thread.</Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {tickets.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Loading tickets…</Trans>
          </p>
        ) : tickets.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load tickets:</Trans> {tickets.error.message}
          </p>
        ) : rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>No tickets yet.</Trans>
          </p>
        ) : (
          <ul data-testid="ticket-list" className="space-y-1.5">
            {rows.map((ticket) => (
              <li key={ticket.ticket_id}>
                <Button
                  type="button"
                  variant={selected === ticket.ticket_id ? "secondary" : "outline"}
                  className="h-auto w-full justify-between py-2"
                  onClick={(): void => {
                    setSelected(ticket.ticket_id);
                  }}
                >
                  <span className="flex items-center gap-2">
                    <code className="font-mono text-xs">{ticket.ticket_id}</code>
                    <span className="truncate">{ticket.subject}</span>
                  </span>
                  <Badge variant="stale">{ticket.severity}</Badge>
                </Button>
              </li>
            ))}
          </ul>
        )}

        {selected !== undefined ? <TicketThread id={selected} /> : null}
      </CardContent>
    </Card>
  );
}

/** The community-support card for the free tier. */
function CommunitySupport({ sla }: { readonly sla: string }): JSX.Element {
  return (
    <Card data-testid="community-support">
      <CardHeader>
        <CardTitle>
          <Trans>Community support</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Help from the community and the documentation. The in-app docs cover
            most setup and troubleshooting.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        <ul className="space-y-1.5">
          <li>
            <a href="/help" className="underline underline-offset-2">
              <Trans>Read the in-app documentation</Trans>
            </a>
          </li>
          <li>
            <a
              href="https://github.com/aperim/multiview/discussions"
              className="underline underline-offset-2"
              rel="noreferrer"
            >
              <Trans>Ask in the community discussions</Trans>
            </a>
          </li>
        </ul>
        <p data-testid="community-quiet-line" className="text-muted-foreground">
          <Trans>
            Your plan includes community best-effort support
          </Trans>{" "}
          (<span className="font-mono">{sla}</span>).{" "}
          <Trans>A paid plan adds an entitled support queue.</Trans>
        </p>
      </CardContent>
    </Card>
  );
}

/** The context-pack composer: window + include[] → redacted bundle preview. */
function ContextPackComposer(): JSX.Element {
  const { t } = useLingui();
  const [window, setWindow] = useState<BundleWindow>("24h");
  const [include, setInclude] = useState<Set<BundleInclude>>(new Set());
  const [working, setWorking] = useState(false);
  const [bundle, setBundle] = useState<Bundle | undefined>(undefined);

  const toggle = (section: BundleInclude): void => {
    setInclude((current) => {
      const next = new Set(current);
      if (next.has(section)) {
        next.delete(section);
      } else {
        next.add(section);
      }
      return next;
    });
  };

  const compose = async (): Promise<void> => {
    setWorking(true);
    setBundle(undefined);
    try {
      const accepted = await composeBundle({ window, include: Array.from(include) });
      const preview = await getBundle(accepted.bundle_id);
      setBundle(preview);
    } catch (error) {
      toast({
        title: t`Could not compose the bundle`,
        description: error instanceof Error ? error.message : t`Unknown error`,
        variant: "destructive",
      });
    } finally {
      setWorking(false);
    }
  };

  return (
    <Card data-testid="context-pack-composer">
      <CardHeader>
        <CardTitle>
          <Trans>Context pack</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Compose a redacted diagnostics pack to attach to a ticket. Preview it
            here first — nothing leaves the machine until you attach it.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid gap-1.5">
          <Label htmlFor="bundle-window">
            <Trans>Window</Trans>
          </Label>
          <select
            id="bundle-window"
            className="h-9 min-h-[44px] w-32 rounded-md border border-input bg-background px-3 text-sm"
            value={window}
            onChange={(e): void => {
              setWindow(toWindow(e.target.value));
            }}
          >
            {WINDOWS.map((w) => (
              <option key={w} value={w}>
                {w}
              </option>
            ))}
          </select>
        </div>

        <fieldset className="space-y-1.5">
          <legend className="text-sm font-medium">
            <Trans>Include</Trans>
          </legend>
          {INCLUDES.map((section) => (
            <label key={section} className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                className="size-4"
                checked={include.has(section)}
                onChange={(): void => {
                  toggle(section);
                }}
              />
              <span className="capitalize">{section}</span>
            </label>
          ))}
        </fieldset>

        <Button
          type="button"
          disabled={working || include.size === 0}
          onClick={(): void => {
            void compose();
          }}
        >
          <Trans>Compose preview</Trans>
        </Button>

        <p data-testid="never-media-statement" className="text-sm text-muted-foreground">
          <Trans>
            A context pack carries diagnostics, logs, and a redacted config — never
            media, never raw identifiers, never secret values. It leaves the machine
            only when you explicitly attach it to a ticket.
          </Trans>
        </p>

        {bundle !== undefined ? (
          <div data-testid="bundle-preview" className="rounded-md border p-3">
            <p className="mb-2 text-sm font-medium">
              <Trans>Redactions in this pack</Trans>
            </p>
            {bundle.redactions.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                <Trans>Nothing was redacted.</Trans>
              </p>
            ) : (
              <ul className="space-y-1 text-sm">
                {bundle.redactions.map((redaction) => (
                  <li key={redaction.path} className="flex items-center gap-2">
                    <Badge variant="stale">{redaction.reason}</Badge>
                    <code className="font-mono text-xs">{redaction.path}</code>
                  </li>
                ))}
              </ul>
            )}
            <p className="mt-2 text-xs text-muted-foreground">
              <Trans>Each entry is a location that was masked — never the value.</Trans>
            </p>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** The Support screen. */
export function SupportPage(): JSX.Element {
  const entitlement = useSupportEntitlement();

  return (
    <>
      <PageHeader
        title={<Trans>Support</Trans>}
        description={
          <Trans>
            Raise a ticket, read your threads, and compose a redacted context pack.
          </Trans>
        }
      />

      {entitlement.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading support options…</Trans>
        </p>
      ) : entitlement.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load support options:</Trans> {entitlement.error.message}
        </p>
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          {entitlement.data.eligible ? (
            <>
              <RaiseTicketForm routeTo={entitlement.data.route.to} />
              <TicketsPanel />
            </>
          ) : (
            <CommunitySupport sla={entitlement.data.sla} />
          )}
          <div className="lg:col-span-2">
            <ContextPackComposer />
          </div>
        </div>
      )}
    </>
  );
}
