import { fireEvent, render, screen } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import SwarmPanel from "./SwarmPanel";
import type { SwarmState } from "../lib/swarm";

function sampleSwarm(): SwarmState {
  return {
    swarmId: "sw-412",
    title: "Release checks",
    goal: {
      title: "Ship read-only swarm",
      line: "Daemon, control plane, Web PWA, CLI",
    },
    threads: [
      {
        id: "t-core",
        title: "Core and daemon",
        goal: "Watch persistence files read-only",
        tasks: ["worker-daemon", "verifier"],
      },
    ],
    tasks: {
      "worker-daemon": "Implement read-only watcher",
      verifier: "Run verification gates",
    },
    members: [
      {
        name: "coordinator",
        role: "coordinator",
        model: "Gemini 4 Neon (High)",
        workspace_mode: "inherit",
        state: "running",
        phase: "working",
        last_read_seq: 2,
        killed: false,
      },
      {
        name: "worker-daemon",
        role: "worker",
        model: "Gemini 4 Hydrogen (Medium)",
        workspace_mode: "share",
        state: "running",
        phase: "waiting",
        last_read_seq: 1,
        killed: false,
      },
    ],
    messages: [
      {
        seq: 1,
        agent: "coordinator",
        text: "@worker-daemon start the watcher",
        time: "2026-11-24T13:55:00Z",
      },
      {
        seq: 2,
        agent: "worker-daemon",
        text: "@coordinator watcher ready",
        time: "2026-11-24T13:56:00Z",
        kind: "status",
        truncated: true,
      },
    ],
  };
}

describe("SwarmPanel", () => {
  it("renders goal, threads, member roster, and message board with @mentions", () => {
    render(() => <SwarmPanel swarm={sampleSwarm()} />);

    expect(screen.getByText("Release checks")).toBeInTheDocument();
    expect(screen.getByText("Ship read-only swarm")).toBeInTheDocument();
    expect(screen.getByText("Core and daemon")).toBeInTheDocument();
    expect(screen.getByText("Implement read-only watcher")).toBeInTheDocument();
    expect(screen.getByText("Gemini 4 Neon (High)")).toBeInTheDocument();
    expect(screen.getByText("share")).toBeInTheDocument();
    expect(screen.getByText("#1")).toBeInTheDocument();
    expect(screen.getByText("@worker-daemon")).toBeInTheDocument();
    expect(screen.getByText("truncated")).toBeInTheDocument();
  });

  it("collapses and expands on header click", () => {
    render(() => <SwarmPanel swarm={sampleSwarm()} />);

    const toggle = screen.getByRole("button", { name: /Release checks/i });
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("Ship read-only swarm")).toBeInTheDocument();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("Ship read-only swarm")).not.toBeInTheDocument();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("Ship read-only swarm")).toBeInTheDocument();
  });
});
