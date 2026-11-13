import { within } from "@testing-library/react";
import type { EventRecord } from "./EventLog";
import { mock } from "./test-bridge";
import type {
  Dashboard,
  UsageFilter,
  UsagePage,
  UsageRecord,
  UsageState,
} from "./usage-types";

/** Usage settings with recording and automatic sync switched on. */
export const usageState: UsageState = {
  settings: {
    recording: true,
    autoSync: true,
    refreshSeconds: 0,
    multiplier: "1",
    pricingModel: "response",
  },
  syncing: false,
  reports: {},
  error: null,
};

/** A dashboard for `requests` successful requests that have no price. */
export function usageDashboard(
  requests: number,
  totals: Partial<Dashboard["totals"]> = {},
): Dashboard {
  return {
    totals: {
      requests,
      attempts: requests,
      success: requests,
      statusKnown: requests,
      sessions: 0,
      tokens: { input: 1000, output: 2000, cacheRead: 3000, cacheWrite: 4000 },
      cost: "0",
      unpriced: requests,
      durationMs: 0,
      measuredOutputs: 0,
      generationMs: 0,
      ...totals,
    },
    providers: [],
    models: [],
    detailSince: 0,
    sources: {},
  };
}

/**
 * Answers the usage page's commands from the given fixtures, twenty rows to a
 * page. Any other command fails the test.
 */
export function serveUsage(
  records: () => UsageRecord[],
  dashboard: () => Dashboard,
) {
  mock.command.mockReset();
  mock.listeners.clear();
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      switch (name) {
        case "get_gateway":
          return {
            providers: [{ id: "fixture-provider", name: "Fixture Provider" }],
          };
        case "get_usage_state":
          return structuredClone(usageState);
        case "get_usage_dashboard":
          return dashboard();
        case "get_usage_logs": {
          const filter = args.filter as UsageFilter;
          const page = filter.page ?? 1;
          const rows = records().slice((page - 1) * 20, page * 20);
          return {
            rows: structuredClone(rows),
            total: records().length,
            page,
            detailSince: 0,
          } satisfies UsagePage;
        }
        case "get_usage_detail":
          return structuredClone(records().find((item) => item.id === args.id));
        default:
          throw new Error(`Unexpected fixture command: ${name}`);
      }
    },
  );
}

/** The value beside a label in a detail dialog, outside its collapsed sections. */
export function detailValue(dialog: HTMLElement, label: string): HTMLElement {
  const dt = within(dialog)
    .getAllByText(label, { selector: "dt" })
    .find((candidate) => !candidate.closest("details"));
  if (!dt) throw new Error(`主详情区缺少字段：${label}`);
  return dt.nextElementSibling as HTMLElement;
}

/**
 * Answers the event log's commands from the given records. `other` answers the
 * commands only one test file needs.
 */
export function serveEvents(
  records: () => EventRecord[],
  other: (name: string) => unknown = () => undefined,
) {
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "get_gateway") {
        return {
          clientId: args.clientId,
          providers: [{ id: "provider-1", name: "Fixture Provider" }],
        };
      }
      if (name === "get_app_events") {
        return {
          items: structuredClone(records()),
          total: records().length,
          page: 1,
          error: null,
        };
      }
      if (name === "get_app_event") {
        return records().find((item) => item.id === args.id) ?? null;
      }
      return other(name);
    },
  );
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
}
