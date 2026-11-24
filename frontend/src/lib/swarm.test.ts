import { describe, expect, it } from "vitest";
import type { TimedEvent } from "../api/relay";
import {
  activeMemberCount,
  foldSwarmState,
  memberStatusLabel,
  memberStatusTone,
  segmentMentions,
} from "./swarm";

describe("foldSwarmState", () => {
  it("returns null when no swarm events exist", () => {
    const events: TimedEvent[] = [
      { event: { type: "started", harness_session_id: "s-1" }, atUnix: 100 },
    ];
    expect(foldSwarmState(events)).toBeNull();
  });

  it("folds snapshots and deduplicates board messages in ascending seq order", () => {
    const events: TimedEvent[] = [
      {
        atUnix: 100,
        event: {
          type: "swarm_updated",
          snapshot: {
            swarm_id: "sw-1",
            title: "Initial",
            goal: { title: "Ship 412", line: "Read-only swarm" },
            threads: [{ id: "t1", title: "Core", tasks: ["worker-1"] }],
            tasks: { "worker-1": "Wire protocol" },
            members: [
              {
                name: "coordinator",
                role: "coordinator",
                model: "Gemini 4 Neon (High)",
                phase: "working",
              },
              {
                name: "worker-1",
                role: "worker",
                model: "Gemini 4 Hydrogen (Medium)",
                workspace_mode: "share",
                phase: "waiting",
              },
            ],
          },
          board_messages: [
            { seq: 2, agent: "worker-1", text: "@coordinator ready" },
            { seq: 1, agent: "coordinator", text: "@worker-1 start" },
          ],
        },
      },
      {
        atUnix: 105,
        event: {
          type: "swarm_updated",
          board_messages: [
            // Overlapping seq 2 + new seq 3.
            { seq: 2, agent: "worker-1", text: "@coordinator ready" },
            { seq: 3, agent: "worker-1", text: "@coordinator [done] wire merged" },
          ],
        },
      },
    ];

    const state = foldSwarmState(events);
    expect(state).not.toBeNull();
    expect(state?.title).toBe("Initial");
    expect(state?.goal?.title).toBe("Ship 412");
    expect(state?.threads).toHaveLength(1);
    expect(state?.members).toHaveLength(2);
    expect(activeMemberCount(state?.members ?? [])).toBe(1);
    expect(state?.messages.map((m) => m.seq)).toEqual([1, 2, 3]);
  });

  it("replaces snapshot state when a newer snapshot arrives", () => {
    const events: TimedEvent[] = [
      {
        atUnix: 100,
        event: {
          type: "swarm_updated",
          snapshot: {
            title: "Phase 1",
            members: [{ name: "a", phase: "working" }],
          },
        },
      },
      {
        atUnix: 110,
        event: {
          type: "swarm_updated",
          snapshot: {
            title: "Phase 2",
            members: [{ name: "a", phase: "waiting", killed: true }],
          },
        },
      },
    ];

    const state = foldSwarmState(events);
    expect(state?.title).toBe("Phase 2");
    expect(memberStatusTone(state!.members[0]!)).toBe("killed");
    expect(memberStatusLabel(state!.members[0]!)).toBe("killed");
  });
});

describe("segmentMentions", () => {
  it("highlights @mentions without touching emails", () => {
    expect(segmentMentions("@coordinator check @worker-1 and me@lexo.cool")).toEqual([
      { kind: "mention", text: "@coordinator" },
      { kind: "text", text: " check " },
      { kind: "mention", text: "@worker-1" },
      { kind: "text", text: " and me@lexo.cool" },
    ]);
  });
});
