import { expectTypeOf, it } from "vitest";
import { command, subscribe } from "./bridge";
import type { CommandName, CommandResult, EventMap } from "./bridge-contracts";
import type { AppError, GatewayState, ViewState } from "./types";
import type { ConfigDocument } from "./types";
import type { UsageRecord } from "./usage-types";

function commandTypeExamples() {
  const state = command("get_state");
  expectTypeOf(state).toEqualTypeOf<Promise<ViewState>>();

  const config = command("read_config");
  expectTypeOf(config).toEqualTypeOf<Promise<ConfigDocument>>();

  const detail = command("get_usage_detail", { id: "fixture-record" });
  expectTypeOf(detail).toEqualTypeOf<Promise<UsageRecord>>();

  // @ts-expect-error Unknown commands must not be callable.
  void command("unknown_fixture_command");

  // @ts-expect-error Required command arguments cannot be omitted.
  void command("switch_account");

  void command("switch_account", {
    // @ts-expect-error The account id must be a string.
    id: 1,
    expectedRevision: "fixture-revision",
  });

  // @ts-expect-error Command arguments must match the command's declared shape.
  void command("pick_path", { kind: 1 });

  // @ts-expect-error Commands with no arguments reject unexpected fields.
  void command("get_state", { fixture: true });
}

function eventTypeExamples() {
  void subscribe("switch-notice", (payload) => {
    expectTypeOf(payload).toEqualTypeOf<string>();
  });
  void subscribe("switch-error", (payload) => {
    expectTypeOf(payload).toEqualTypeOf<AppError>();
  });
  void subscribe("gateway-state", (payload) => {
    expectTypeOf(payload).toEqualTypeOf<GatewayState>();
  });

  // @ts-expect-error Unknown event names must not be subscribable.
  void subscribe("unknown_fixture_event", () => {});

  // @ts-expect-error Event callbacks receive the declared payload type.
  void subscribe("switch-notice", (payload: number) => void payload);
}

it("keeps command argument, result, and event payload types precise", () => {
  expectTypeOf<CommandName>().not.toEqualTypeOf<"unknown_fixture_command">();
  expectTypeOf<CommandResult<"get_state">>().toEqualTypeOf<ViewState>();
  expectTypeOf<CommandResult<"read_config">>().toEqualTypeOf<ConfigDocument>();
  expectTypeOf<EventMap["switch-notice"]>().toEqualTypeOf<string>();
  expectTypeOf<EventMap["switch-error"]>().toEqualTypeOf<AppError>();
  expectTypeOf<EventMap["gateway-state"]>().toEqualTypeOf<GatewayState>();
});
