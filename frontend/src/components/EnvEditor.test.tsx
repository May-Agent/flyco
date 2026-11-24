import { describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import EnvEditor from "./EnvEditor";

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("EnvEditor", () => {
  it("loads existing .env entries asynchronously via createEffect", async () => {
    vi.mocked(fetch).mockImplementation((input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      const method = (init?.method ?? "GET").toUpperCase();
      if (method === "GET" && url.pathname === "/v1/sessions/s-1/env") {
        return Promise.resolve(
          json({
            entries: [
              { key: "API_KEY", value: "secret-1" },
              { key: "NODE_ENV", value: "production" },
            ],
            warning: "Agents can read this file but not edit it.",
          }),
        );
      }
      return Promise.resolve(json({}, 404));
    });

    const { findByDisplayValue, getByText } = render(() => <EnvEditor sessionId="s-1" />);

    expect(await findByDisplayValue("API_KEY")).toBeInTheDocument();
    expect(await findByDisplayValue("secret-1")).toBeInTheDocument();
    expect(await findByDisplayValue("NODE_ENV")).toBeInTheDocument();
    expect(getByText("Agents can read this file but not edit it.")).toBeInTheDocument();
  });

  it("guards double-submit during Save and syncs state from putSessionEnv response", async () => {
    let resolvePut!: (res: Response) => void;
    const puts: unknown[] = [];

    vi.mocked(fetch).mockImplementation((input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      const method = (init?.method ?? "GET").toUpperCase();
      if (method === "GET" && url.pathname === "/v1/sessions/s-1/env") {
        return Promise.resolve(
          json({
            entries: [{ key: "EXISTING", value: "keep-me" }],
            warning: "Warning text",
          }),
        );
      }
      if (method === "PUT" && url.pathname === "/v1/sessions/s-1/env") {
        puts.push(JSON.parse(String(init?.body)));
        return new Promise<Response>((resolve) => {
          resolvePut = resolve;
        });
      }
      return Promise.resolve(json({}, 404));
    });

    const { findByDisplayValue, getByRole, queryByDisplayValue } = render(() => (
      <EnvEditor sessionId="s-1" />
    ));

    await findByDisplayValue("EXISTING");

    fireEvent.click(getByRole("button", { name: "Add variable" }));
    const keyInputs = document.querySelectorAll('input[aria-label="Variable name"]');
    const valInputs = document.querySelectorAll('input[aria-label="Variable value"]');
    fireEvent.input(keyInputs[1]!, { target: { value: "ADDED" } });
    fireEvent.input(valInputs[1]!, { target: { value: "val-2" } });

    const saveBtn = getByRole("button", { name: "Save" });
    fireEvent.click(saveBtn);
    fireEvent.click(saveBtn);

    expect(puts).toHaveLength(1);
    expect(puts[0]).toEqual({
      entries: [
        { key: "EXISTING", value: "keep-me" },
        { key: "ADDED", value: "val-2" },
      ],
    });

    resolvePut(
      json({
        entries: [
          { key: "EXISTING", value: "keep-me" },
          { key: "ADDED", value: "normalized-val" },
        ],
        warning: "Updated warning",
      }),
    );

    expect(await findByDisplayValue("normalized-val")).toBeInTheDocument();
    expect(queryByDisplayValue("val-2")).toBeNull();
  });

  it("re-seeds entries when switching sessionId", async () => {
    vi.mocked(fetch).mockImplementation((input) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (url.pathname === "/v1/sessions/s-1/env") {
        return Promise.resolve(
          json({ entries: [{ key: "FIRST", value: "1" }], warning: "w" }),
        );
      }
      if (url.pathname === "/v1/sessions/s-2/env") {
        return Promise.resolve(
          json({ entries: [{ key: "SECOND", value: "2" }], warning: "w" }),
        );
      }
      return Promise.resolve(json({}, 404));
    });

    const [sessionId, setSessionId] = createSignal("s-1");
    const { findByDisplayValue, queryByDisplayValue } = render(() => (
      <EnvEditor sessionId={sessionId()} />
    ));

    expect(await findByDisplayValue("FIRST")).toBeInTheDocument();
    setSessionId("s-2");
    expect(await findByDisplayValue("SECOND")).toBeInTheDocument();
    await waitFor(() => expect(queryByDisplayValue("FIRST")).toBeNull());
  });
});
