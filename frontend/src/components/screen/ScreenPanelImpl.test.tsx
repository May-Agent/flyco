import { afterEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { fireEvent, render } from "@solidjs/testing-library";
import type { SessionRelay } from "../../api/relay";

const screenState = vi.hoisted(() => ({
  supported: true,
  watcher: 10 as number | null,
  feedConstructed: 0,
  takeoverCalls: [] as { watcher: number; active: boolean }[],
  inputCalls: [] as unknown[],
  disposeFeed: vi.fn(),
  helloCb: null as ((h: { watcher: number; takeover: boolean }) => void) | null,
}));

vi.mock("../../lib/desktopDecoder", () => ({
  desktopDecodeSupported: vi.fn(() => Promise.resolve(screenState.supported)),
  DesktopDecoder: class MockDecoder {
    push() {}
    reset() {}
    dispose() {}
  },
}));

vi.mock("../../api/desktop", () => ({
  createDesktopFeed: vi.fn((_sessionId: string, options: { listener: { hello: (h: { watcher: number; takeover: boolean }) => void } }) => {
    screenState.feedConstructed += 1;
    screenState.helloCb = options.listener.hello;
    return {
      state: () => "live",
      watcher: () => screenState.watcher,
      failure: () => null,
      dispose: screenState.disposeFeed,
    };
  }),
}));

vi.mock("../../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../api/client")>();
  return {
    ...actual,
    desktopTakeover: vi.fn((_id: string, req: { watcher: number; active: boolean }) => {
      screenState.takeoverCalls.push(req);
      return Promise.resolve();
    }),
    desktopInput: vi.fn((_id: string, req: unknown) => {
      screenState.inputCalls.push(req);
      return Promise.resolve();
    }),
  };
});

import ScreenPanelImpl from "./ScreenPanelImpl";
import * as client from "../../api/client";

const relay: SessionRelay = {
  state: () => "live",
  failure: () => null,
  events: () => [],
  send: vi.fn(() => Promise.resolve()),
  dispose: vi.fn(),
};

afterEach(() => {
  vi.clearAllMocks();
  screenState.supported = true;
  screenState.watcher = 10;
  screenState.feedConstructed = 0;
  screenState.takeoverCalls = [];
  screenState.inputCalls = [];
  screenState.helloCb = null;
});

describe("ScreenPanelImpl", () => {
  it("does not construct DesktopFeed when WebCodecs is unsupported", async () => {
    screenState.supported = false;
    const { findByText } = render(() => (
      <ScreenPanelImpl sessionId="s-1" relay={relay} onError={vi.fn()} />
    ));

    expect(
      await findByText(/cannot decode AV1/),
    ).toBeInTheDocument();
    expect(screenState.feedConstructed).toBe(0);
  });

  it("constructs DesktopFeed only after support resolves true and focuses stage on pointerdown while driving", async () => {
    const { findByRole } = render(() => (
      <ScreenPanelImpl sessionId="s-1" relay={relay} onError={vi.fn()} />
    ));

    await vi.waitFor(() => expect(screenState.feedConstructed).toBe(1));
    const takeOver = await findByRole("button", { name: "Take over" });
    fireEvent.click(takeOver);

    const releaseBtn = await findByRole("button", { name: "Release" });
    expect(releaseBtn).toBeInTheDocument();

    const stage = releaseBtn.closest("div")!.nextElementSibling as HTMLElement;
    const focusSpy = vi.spyOn(stage, "focus");
    stage.setPointerCapture = vi.fn();
    fireEvent.pointerDown(stage, { pointerId: 1, button: 0 });
    expect(focusSpy).toHaveBeenCalled();
  });

  it("ignores stale takeOver resolution when watcher changes mid-flight and guards release with takeoverBusy", async () => {
    let resolveTakeover!: () => void;
    const [watcher, setWatcher] = createSignal<number | null>(10);

    const { createDesktopFeed } = await import("../../api/desktop");
    vi.mocked(createDesktopFeed).mockImplementationOnce((_id, options) => {
      screenState.feedConstructed += 1;
      screenState.helloCb = options.listener.hello;
      return {
        state: () => "live",
        watcher,
        failure: () => null,
        dispose: screenState.disposeFeed,
      };
    });

    vi.mocked(client.desktopTakeover).mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          resolveTakeover = resolve;
        }),
    );

    const { findByRole, queryByRole } = render(() => (
      <ScreenPanelImpl sessionId="s-1" relay={relay} onError={vi.fn()} />
    ));

    await vi.waitFor(() => expect(screenState.feedConstructed).toBe(1));
    fireEvent.click(await findByRole("button", { name: "Take over" }));

    // Watcher changes before takeover resolves.
    setWatcher(11);
    resolveTakeover();
    await Promise.resolve();
    await Promise.resolve();

    // Must not flip to Release/driving for the superseded watcher.
    expect(queryByRole("button", { name: "Release" })).toBeNull();
  });

  it("serializes desktopInput POSTs instead of sending concurrent batches", async () => {
    let resolveInput!: () => void;
    vi.mocked(client.desktopInput).mockImplementationOnce(
      (_id, req) => {
        screenState.inputCalls.push(req);
        return new Promise<void>((resolve) => {
          resolveInput = resolve;
        });
      },
    );

    const { findByRole } = render(() => (
      <ScreenPanelImpl sessionId="s-1" relay={relay} onError={vi.fn()} />
    ));

    await vi.waitFor(() => expect(screenState.feedConstructed).toBe(1));
    fireEvent.click(await findByRole("button", { name: "Take over" }));
    const releaseBtn = await findByRole("button", { name: "Release" });
    const stage = releaseBtn.closest("div")!.nextElementSibling as HTMLElement;

    fireEvent.keyDown(stage, { code: "KeyA", key: "a" });
    fireEvent.keyDown(stage, { code: "KeyB", key: "b" });
    expect(client.desktopInput).toHaveBeenCalledTimes(1);

    resolveInput();
    await vi.waitFor(() => expect(client.desktopInput).toHaveBeenCalledTimes(2));
  });
});
