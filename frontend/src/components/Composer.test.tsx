import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import Composer from "./Composer";
import { ReadinessProvider } from "./Readiness";
import { rememberRepo } from "../lib/localPreferences";

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

/**
 * The composer under the same two providers it has on the home page: a
 * router, because the missing-prerequisite chips are links, and readiness,
 * because every chip reads what is linked. The control plane behind both
 * is the in-memory one from src/test/setup.ts.
 */
function mount() {
  const history = createMemoryHistory();
  history.set({ value: "/", replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history}>
      <Route
        path="/"
        component={() => (
          <ReadinessProvider enabled={() => true}>
            <Composer onSend={vi.fn(() => Promise.resolve())} />
          </ReadinessProvider>
        )}
      />
    </MemoryRouter>
  ));
}

describe("Composer", () => {
  it("asks for repositories while none is chosen", async () => {
    const { findByRole, queryByText } = mount();

    expect(
      await findByRole("button", { name: /Select repositories/ }),
    ).toBeInTheDocument();
    // No branch picker anywhere: a branch is a fact about one repository,
    // and there is no repository yet to have one.
    expect(queryByText("Default branch")).not.toBeInTheDocument();
  });

  it("names the chosen repository, and its branch inside the picker", async () => {
    // The most recent repository is preselected, which is how a returning
    // user lands on the page with one already chosen.
    rememberRepo("octocat/hello-world");
    const { findByRole } = mount();

    const chip = await findByRole("button", { name: /octocat\/hello-world/ });
    chip.click();

    // The picked row's branch is read as soon as it is known, so the picker
    // names it rather than saying "Default branch".
    expect(await findByRole("button", { name: "main" })).toBeInTheDocument();
  });

  it("defers RepoChip fetch until opened and debounces repository search by 250ms", async () => {
    vi.useFakeTimers();
    const repoQueries: string[] = [];
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation((input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (url.pathname === "/v1/github/repos") {
        repoQueries.push(url.searchParams.get("q") ?? "");
        return Promise.resolve(
          new Response(JSON.stringify([{ slug: "octocat/hello-world", private: false }]), {
            status: 200,
            headers: { "content-type": "application/json" },
          }),
        );
      }
      return base!(input, init);
    });

    const { getByRole, getByLabelText } = mount();
    await vi.advanceTimersByTimeAsync(100);
    // Deferred until the popover opens.
    expect(repoQueries).toEqual([]);

    fireEvent.click(getByRole("button", { name: /Select repositories/ }));
    await vi.advanceTimersByTimeAsync(0);
    expect(repoQueries).toEqual([""]);

    const search = getByLabelText("Search your repositories");
    fireEvent.input(search, { target: { value: "f" } });
    fireEvent.input(search, { target: { value: "fl" } });
    fireEvent.input(search, { target: { value: "fly" } });

    await vi.advanceTimersByTimeAsync(249);
    expect(repoQueries).toEqual([""]);
    await vi.advanceTimersByTimeAsync(1);
    await Promise.resolve();
    expect(repoQueries).toEqual(["", "fly"]);
  });

  it("guards Enter-key double-submit while sending is in flight", async () => {
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation((input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (url.pathname === "/v1/harness-accounts") {
        return Promise.resolve(
          new Response(
            JSON.stringify([
              {
                id: "h-1",
                harness: "claude_code",
                label: "claude",
                linked_at_unix: 1,
                expires_at_unix: null,
                models: [],
                usage: { state: "unmetered" },
              },
            ]),
            { status: 200, headers: { "content-type": "application/json" } },
          ),
        );
      }
      if (url.pathname === "/v1/providers") {
        return Promise.resolve(
          new Response(
            JSON.stringify([{ id: "p-1", kind: "aws", label: "AWS", linked_at_unix: 1 }]),
            { status: 200, headers: { "content-type": "application/json" } },
          ),
        );
      }
      return base!(input, init);
    });

    rememberRepo("octocat/hello-world");
    let resolveSend!: () => void;
    const onSend = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          resolveSend = resolve;
        }),
    );

    const history = createMemoryHistory();
    history.set({ value: "/", replace: true, scroll: false });
    const { findByRole, getByLabelText } = render(() => (
      <MemoryRouter history={history}>
        <Route
          path="/"
          component={() => (
            <ReadinessProvider enabled={() => true}>
              <Composer onSend={onSend} />
            </ReadinessProvider>
          )}
        />
      </MemoryRouter>
    ));

    const field = getByLabelText("Describe a task") as HTMLTextAreaElement;
    fireEvent.input(field, { target: { value: "Build it" } });
    await vi.waitFor(async () => expect(await findByRole("button", { name: "Start session" })).toBeEnabled());

    fireEvent.keyDown(field, { key: "Enter" });
    expect(field).toBeDisabled();
    fireEvent.keyDown(field, { key: "Enter" });
    expect(onSend).toHaveBeenCalledTimes(1);

    resolveSend();
  });
});
