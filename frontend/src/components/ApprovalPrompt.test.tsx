import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import ApprovalPrompt, { type PendingApproval } from "./ApprovalPrompt";

const T0 = 1_800_000_000;

function approval(payload: PendingApproval["payload"]): PendingApproval {
  return {
    kind: "approval",
    key: "approval-ap-1",
    id: "ap-1",
    payload,
    state: "pending",
    atUnix: T0,
  };
}

describe("ApprovalPrompt", () => {
  it("reads a tool call's arguments out, one row each, instead of printing JSON", () => {
    const onDecide = vi.fn();
    const { getByRole, getByText, queryByText } = render(() => (
      <ApprovalPrompt
        approval={approval({
          kind: "tool_use",
          tool: "Bash",
          input: { command: "git push --force origin fix/flaky-compaction" },
        })}
        remaining={1}
        deciding={false}
        onDecide={onDecide}
      />
    ));

    const prompt = getByRole("region", { name: "Approval" });
    expect(prompt.textContent).toContain("Run Bash");
    expect(getByText("command")).toBeInTheDocument();
    expect(getByText("git push --force origin fix/flaky-compaction")).toBeInTheDocument();
    expect(queryByText(/^\{$/)).not.toBeInTheDocument();
    expect(prompt.querySelector("dl")).not.toBeNull();
    expect(queryByText("1 / 1")).not.toBeInTheDocument();

    fireEvent.click(getByRole("button", { name: "Approve" }));
    expect(onDecide).toHaveBeenCalledWith("ap-1", "approved");
  });

  it("renders structured sentence approvals and invokes deny", () => {
    const onDecide = vi.fn();
    const { getByRole } = render(() => (
      <ApprovalPrompt
        approval={approval({
          kind: "merge",
          repo: "May-Agent/flyco",
          from_branch: "fix/flaky-compaction",
          into_branch: "main",
        })}
        remaining={3}
        deciding={false}
        onDecide={onDecide}
      />
    ));

    const prompt = getByRole("region", { name: "Approval" });
    expect(prompt.textContent).toContain("May-Agent/flyco: fix/flaky-compaction → main");
    expect(prompt.textContent).toContain("1 / 3");
    expect(prompt.querySelector("dl")).toBeNull();

    fireEvent.click(getByRole("button", { name: "Deny" }));
    expect(onDecide).toHaveBeenCalledWith("ap-1", "denied");
  });

  it("disables both buttons while a decision is in flight", () => {
    const { getByRole } = render(() => (
      <ApprovalPrompt
        approval={approval({
          kind: "tool_use",
          tool: "Bash",
          input: { command: "uname -s -m" },
        })}
        remaining={1}
        deciding={true}
        onDecide={() => {}}
      />
    ));

    expect(getByRole("button", { name: "Approve" })).toBeDisabled();
    expect(getByRole("button", { name: "Deny" })).toBeDisabled();
  });
});
