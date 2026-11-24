import { describe, expect, it } from "vitest";
import { parseClientEvent } from "./wire";

describe("parseClientEvent", () => {
  it("reads a commands frame as the harness sends it", () => {
    // Verbatim rows of what `flyco_core::wire::HarnessCommand` serializes
    // to: `snake_case`, no leading slash on the name, a colon in a plugin's
    // skill, and `null` for a command that takes no argument.
    const frame = {
      type: "commands",
      commands: [
        {
          name: "effort",
          description: "Set effort level for model usage",
          argument_hint: "<low|medium|high|xhigh|max|ultracode|auto>",
        },
        {
          name: "presence:status",
          description: "What presence currently thinks the user is doing",
          argument_hint: null,
        },
      ],
    };

    const event = parseClientEvent(frame);
    expect(event.type).toBe("commands");
    if (event.type !== "commands") {
      throw new Error("the frame is a commands event");
    }
    expect(event.commands[0]?.argument_hint).toBe("<low|medium|high|xhigh|max|ultracode|auto>");
    expect(event.commands[1]?.name).toBe("presence:status");
    expect(event.commands[1]?.argument_hint).toBeNull();
  });

  it("parses computer_use_changed and terminal_exited frames", () => {
    expect(parseClientEvent({ type: "computer_use_changed", enabled: true })).toEqual({
      type: "computer_use_changed",
      enabled: true,
    });
    expect(parseClientEvent({ type: "terminal_exited", code: 0 })).toEqual({
      type: "terminal_exited",
      code: 0,
    });
    expect(parseClientEvent({ type: "terminal_exited", code: null })).toEqual({
      type: "terminal_exited",
      code: null,
    });
  });

  it("parses swarm_updated frames", () => {
    const frame = {
      type: "swarm_updated",
      snapshot: {
        swarm_id: "sw-1",
        title: "Release",
        members: [{ name: "coordinator", role: "coordinator", phase: "working" }],
      },
      board_messages: [
        {
          seq: 1,
          agent: "coordinator",
          text: "@worker begin",
          time: "2026-11-24T13:55:00Z",
        },
      ],
    };
    const parsed = parseClientEvent(frame);
    expect(parsed).toEqual(frame);
  });

  it("refuses a frame this protocol version does not define", () => {
    expect(() => parseClientEvent({ type: "skills" })).toThrow("unrecognized type");
    expect(() => parseClientEvent(null)).toThrow("not a ClientEvent");
  });
});
