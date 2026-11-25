import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import SettingsModal from "./SettingsModal";
import { ReadinessProvider } from "./Readiness";

function mount(onClose = vi.fn()) {
  const history = createMemoryHistory();
  history.set({ value: "/sessions/abc-123", replace: true, scroll: false });
  const view = render(() => (
    <MemoryRouter history={history}>
      <Route
        path="/sessions/:id"
        component={() => (
          <ReadinessProvider enabled={() => true}>
            <SettingsModal onClose={onClose} />
          </ReadinessProvider>
        )}
      />
    </MemoryRouter>
  ));
  return { ...view, history, onClose };
}

describe("SettingsModal", () => {
  it("opens over the active route, switches sections in place, and drills into agent linking inline", async () => {
    const { findByRole, getByRole, history } = mount();

    const dialog = (await findByRole("dialog", { name: "Settings" })) as HTMLDialogElement;
    expect(dialog.open).toBe(true);
    expect(await findByRole("heading", { level: 2, name: "Agents" })).toBeInTheDocument();

    // Switching sections stays on the current session route.
    fireEvent.click(getByRole("button", { name: "API keys" }));
    expect(await findByRole("heading", { level: 2, name: "API keys" })).toBeInTheDocument();
    expect(history.get()).toBe("/sessions/abc-123");

    // Drilling into an agent's Connect flow stays inside the modal rather than navigating away.
    fireEvent.click(getByRole("button", { name: "Agents" }));
    const connectButtons = await findByRole("heading", { level: 2, name: "Agents" }).then(() =>
      dialog.querySelectorAll("button"),
    );
    const firstConnect = Array.from(connectButtons).find((btn) => btn.textContent === "Connect");
    expect(firstConnect).toBeDefined();
    fireEvent.click(firstConnect!);

    expect(await findByRole("button", { name: "Back to Agents" })).toBeInTheDocument();
    expect(history.get()).toBe("/sessions/abc-123");
  });
});
