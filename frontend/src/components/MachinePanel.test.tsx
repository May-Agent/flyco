import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import MachinePanel from "./MachinePanel";
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

describe("MachinePanel", () => {
  it("lays out provider, region, capacity, and rate in structured rows instead of a middot pile", () => {
    const { getByText, queryByText } = render(() => (
      <MachinePanel sessionId="s-1" machine={MACHINE} onChanged={vi.fn()} />
    ));

    expect(getByText("m7i-flex.xlarge")).toBeInTheDocument();
    expect(getByText("Provider")).toBeInTheDocument();
    expect(getByText("AWS")).toBeInTheDocument();
    expect(getByText("Region")).toBeInTheDocument();
    expect(getByText("us-east-1")).toBeInTheDocument();
    expect(getByText("Capacity")).toBeInTheDocument();
    expect(getByText("Spot")).toBeInTheDocument();
    expect(getByText("Rate")).toBeInTheDocument();
    expect(getByText("$0.15/hr")).toBeInTheDocument();
    expect(queryByText(/AWS · us-east-1/)).toBeNull();
  });
});
