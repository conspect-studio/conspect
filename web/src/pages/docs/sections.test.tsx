// Renders every registered docs page and asserts the DOM anchor targets
// (`<section id>`) match the registry exactly — the registry is the single
// source of truth for deep links (ADR-W016), so a section that exists in only
// one of the two is a contract break.
import type { ComponentType, JSX } from "react";
import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";

import { DOCS_REGISTRY, getDocsPage } from "../../docs/registry";
import { TestProviders } from "../../test/render";

type PageLoader = () => Promise<ComponentType>;

// Maps each registered path to its page component. Concept pages are
// route-level lazy chunks in the router; here they are imported directly.
const PAGE_LOADERS: Readonly<Record<string, PageLoader>> = {
  "/help": async () => (await import("./OverviewPage")).OverviewPage,
  "/help/containers": async () => (await import("./ContainerPage")).ContainerPage,
  "/help/compose": async () => (await import("./ComposePage")).ComposePage,
  "/help/config": async () => (await import("./ConfigPage")).ConfigPage,
  "/help/api": async () => (await import("./ApiPage")).ApiPage,
  "/help/features": async () => (await import("./FeaturesPage")).FeaturesPage,
  "/help/devices": async () => (await import("./DevicesHelpPage")).DevicesHelpPage,
  "/help/devices/adopt": async () =>
    (await import("./DevicesAdoptHelpPage")).DevicesAdoptHelpPage,
  "/help/display-nodes": async () =>
    (await import("./DisplayNodesHelpPage")).DisplayNodesHelpPage,
  "/help/sync": async () => (await import("./SyncHelpPage")).SyncHelpPage,
  "/help/concepts/transports": async () =>
    (await import("./concepts/TransportsPage")).TransportsPage,
  "/help/concepts/timing-sync": async () =>
    (await import("./concepts/TimingSyncPage")).TimingSyncPage,
  "/help/concepts/codecs": async () =>
    (await import("./concepts/CodecsPage")).CodecsPage,
  "/help/concepts/color": async () =>
    (await import("./concepts/ColorPage")).ColorPage,
  "/help/concepts/resilience": async () =>
    (await import("./concepts/ResiliencePage")).ResiliencePage,
  "/help/concepts/latency": async () =>
    (await import("./concepts/LatencyPage")).LatencyPage,
  "/help/concepts/glossary": async () =>
    (await import("./concepts/GlossaryPage")).GlossaryPage,
};

describe("docs pages render the registry's anchor targets", () => {
  it("has a loader for every registered page (and no extras)", () => {
    expect(Object.keys(PAGE_LOADERS).sort()).toEqual(
      DOCS_REGISTRY.map((page) => page.path).sort(),
    );
  });

  it.each(DOCS_REGISTRY.map((page) => [page.path] as const))(
    "%s renders exactly its registered sections",
    async (path) => {
      const loader = PAGE_LOADERS[path];
      expect(loader, `no loader for ${path}`).toBeDefined();
      if (loader === undefined) {
        return;
      }
      const Page = await loader();
      const Wrapped = (): JSX.Element => (
        <MemoryRouter initialEntries={[path]}>
          <Page />
        </MemoryRouter>
      );
      const { container } = render(<Wrapped />, { wrapper: TestProviders });
      const renderedIds = Array.from(
        container.querySelectorAll("section[id]"),
        (section) => section.id,
      ).sort();
      const registeredIds = (getDocsPage(path)?.sections ?? [])
        .map((section) => section.id)
        .slice()
        .sort();
      expect(renderedIds).toEqual(registeredIds);
    },
  );
});
