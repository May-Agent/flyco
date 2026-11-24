import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import SkillCatalog from "./SkillCatalog";

function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function mount() {
  const history = createMemoryHistory();
  history.set({ value: "/settings/tools/skill-catalog", replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history}>
      <Route path="/settings/tools/skill-catalog" component={SkillCatalog} />
    </MemoryRouter>
  ));
}

describe("SkillCatalog pending polling", () => {
  it("keeps a fixed 2-minute deadline across repeated pending responses and stops", async () => {
    vi.useFakeTimers();
    let catalogCalls = 0;

    vi.mocked(fetch).mockImplementation((input) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (url.pathname === "/v1/catalog/skills") {
        catalogCalls += 1;
        return Promise.resolve(
          json({
            skills: [],
            pending: ["owner/repo"],
            failed: [],
          }),
        );
      }
      if (url.pathname === "/v1/marketplaces") {
        return Promise.resolve(json([]));
      }
      return Promise.resolve(json({}, 404));
    });

    const { getByText } = mount();
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();
    expect(catalogCalls).toBe(1);

    // Repeated responses with pending must not reset the 2-minute deadline.
    await vi.advanceTimersByTimeAsync(120_000);
    await Promise.resolve();
    const callsAtDeadline = catalogCalls;
    expect(getByText(/taking longer than it should/)).toBeInTheDocument();

    // No further polling after deadline.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(catalogCalls).toBe(callsAtDeadline);
  });
});
