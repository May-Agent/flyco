import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import SessionPlusMenu from "./SessionPlusMenu";
import type { MachineView } from "../api/client";

const MACHINE: MachineView = {
  id: "m-1",
  session: "s-1",
  region: "us-east-1",
  spot: true,
  state: "running",
  hourly: 151_200,
  storage_hourly: 7_040,
  created_at_unix: 1_790_000_000,
  spec: {
    provider: "aws",
    machine_type: "m7i-flex.xlarge",
    region: "us-east-1",
    disk_gib: 64,
    spot: true,
  },
};

describe("SessionPlusMenu", () => {
  it("consolidates session tools behind `+` and drills into budget and goal subviews", async () => {
    const onSetBudget = vi.fn();
    const onSetGoal = vi.fn();
    const onOpenEnv = vi.fn();

    const { getByRole, findByRole } = render(() => (
      <SessionPlusMenu
        sessionId="s-1"
        machine={MACHINE}
        onMachineChanged={vi.fn()}
        budgetSpentUsd={0.25}
        budgetLimitUsd={10}
        savingBudget={false}
        onSetBudget={onSetBudget}
        repos={[{ slug: "octocat/hello-world", branch: "main", dir: "hello-world", added_by: "user" }]}
        onOpenEnv={onOpenEnv}
        onSetGoal={onSetGoal}
      />
    ));

    fireEvent.click(getByRole("button", { name: "Session tools" }));
    expect(await findByRole("menuitem", { name: /Machine/ })).toHaveTextContent("m7i-flex.xlarge");

    fireEvent.click(getByRole("menuitem", { name: /Compute budget/ }));
    const slider = await findByRole("slider", { name: "Session budget in dollars" });
    fireEvent.input(slider, { target: { value: "25" } });
    fireEvent.click(getByRole("button", { name: "Set budget to $25" }));
    expect(onSetBudget).toHaveBeenCalledWith(25);
  });
});
