import { afterEach, describe, expect, it, vi } from "vitest";
import type { RenderOptions, TurnstileApi } from "./turnstile";

describe("acquireTurnstileToken", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.resetModules();
    vi.restoreAllMocks();
    delete window.turnstile;
    document.body.innerHTML = "";
  });

  function installTurnstileStub() {
    const rendered: { id: string; sitekey: string; options: RenderOptions }[] = [];
    let seq = 0;
    const api: TurnstileApi = {
      render: vi.fn((_container, options) => {
        seq += 1;
        const id = `w-${seq}`;
        rendered.push({ id, sitekey: options.sitekey, options });
        return id;
      }),
      execute: vi.fn(),
      reset: vi.fn(),
      remove: vi.fn(),
    };
    window.turnstile = api;
    return { api, rendered };
  }

  it("rejects and unblocks the queue on expired-callback and timeout-callback", async () => {
    const { acquireTurnstileToken } = await import("./turnstile");
    const { rendered } = installTurnstileStub();

    const first = acquireTurnstileToken("key-1");
    await vi.waitFor(() => expect(rendered).toHaveLength(1));
    rendered[0]!.options["expired-callback"]?.();
    await expect(first).rejects.toThrow(/expired/);

    const second = acquireTurnstileToken("key-1");
    await vi.waitFor(() => expect((window.turnstile!.execute as ReturnType<typeof vi.fn>).mock.calls.length).toBe(2));
    rendered[0]!.options["timeout-callback"]?.();
    await expect(second).rejects.toThrow(/timed out/);

    const third = acquireTurnstileToken("key-1");
    await vi.waitFor(() => expect((window.turnstile!.execute as ReturnType<typeof vi.fn>).mock.calls.length).toBe(3));
    rendered[0]!.options.callback?.("tok-ok");
    await expect(third).resolves.toBe("tok-ok");
  });

  it("rejects with a bounded timeout when Turnstile never invokes a callback", async () => {
    vi.useFakeTimers();
    const { acquireTurnstileToken } = await import("./turnstile");
    const { api, rendered } = installTurnstileStub();

    const first = acquireTurnstileToken("key-1", 5000);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    expect(rendered).toHaveLength(1);

    const assertion = expect(first).rejects.toThrow(/timed out/);
    await vi.advanceTimersByTimeAsync(5000);
    await assertion;
    expect(api.reset).toHaveBeenCalledTimes(2);
  });

  it("re-renders the hidden widget when sitekey changes", async () => {
    const { acquireTurnstileToken } = await import("./turnstile");
    const { api, rendered } = installTurnstileStub();

    const first = acquireTurnstileToken("key-a");
    await vi.waitFor(() => expect(rendered).toHaveLength(1));
    rendered[0]!.options.callback?.("tok-a");
    await expect(first).resolves.toBe("tok-a");

    const second = acquireTurnstileToken("key-b");
    await vi.waitFor(() => expect(rendered).toHaveLength(2));
    expect(api.remove).toHaveBeenCalledWith("w-1");
    expect(rendered[1]!.sitekey).toBe("key-b");
    rendered[1]!.options.callback?.("tok-b");
    await expect(second).resolves.toBe("tok-b");
  });
});
