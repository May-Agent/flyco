import { afterEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "@solidjs/testing-library";
import type { SessionRelay } from "../../api/relay";
import type { TimedEvent } from "../../api/relay";

const termState = vi.hoisted(() => ({
  onDataCb: null as ((data: string) => void) | null,
  onResizeCb: null as (() => void) | null,
  cols: 80,
  rows: 24,
  written: [] as string[],
  lines: [] as string[],
  fitCalls: 0,
  resizeObserverCb: null as (() => void) | null,
}));

vi.mock("@xterm/xterm/css/xterm.css", () => ({}));

vi.mock("@xterm/xterm", () => ({
  Terminal: class MockTerminal {
    get cols() {
      return termState.cols;
    }
    get rows() {
      return termState.rows;
    }
    loadAddon() {}
    open() {}
    writeln(line: string) {
      termState.lines.push(line);
    }
    write(data: string) {
      termState.written.push(data);
    }
    onData(cb: (data: string) => void) {
      termState.onDataCb = cb;
      return { dispose: vi.fn() };
    }
    onResize(cb: () => void) {
      termState.onResizeCb = cb;
      return { dispose: vi.fn() };
    }
    dispose() {}
  },
}));

vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class MockFitAddon {
    fit() {
      termState.fitCalls += 1;
    }
  },
}));

import TerminalPanelImpl from "./TerminalPanelImpl";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  termState.onDataCb = null;
  termState.onResizeCb = null;
  termState.cols = 80;
  termState.rows = 24;
  termState.written = [];
  termState.lines = [];
  termState.fitCalls = 0;
  termState.resizeObserverCb = null;
});

function installResizeObserverSpy() {
  vi.stubGlobal(
    "ResizeObserver",
    class MockResizeObserver {
      constructor(cb: () => void) {
        termState.resizeObserverCb = cb;
      }
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
}

describe("TerminalPanelImpl", () => {
  it("coalesces keystrokes and strictly serializes terminal_input POSTs", async () => {
    vi.useFakeTimers();
    installResizeObserverSpy();

    let resolveFirst!: () => void;
    const sent: unknown[] = [];
    const send = vi.fn((cmd: { type: string }) => {
      sent.push(cmd);
      if (cmd.type === "terminal_input" && sent.filter((c) => (c as { type: string }).type === "terminal_input").length === 1) {
        return new Promise<void>((resolve) => {
          resolveFirst = resolve;
        });
      }
      return Promise.resolve();
    });

    const [events, setEvents] = createSignal<TimedEvent[]>([]);
    const relay: SessionRelay = {
      state: () => "live",
      failure: () => null,
      events,
      send,
      dispose: vi.fn(),
    };

    render(() => <TerminalPanelImpl sessionId="s-1" relay={relay} onError={vi.fn()} />);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    expect(termState.onDataCb).not.toBeNull();
    termState.onDataCb!("a");
    termState.onDataCb!("b");
    termState.onDataCb!("c");

    // Coalesced: nothing sent before the 40ms window.
    expect(send).not.toHaveBeenCalledWith({ type: "terminal_input", data: expect.anything() });
    await vi.advanceTimersByTimeAsync(40);

    const inputCalls = () => sent.filter((c) => (c as { type: string }).type === "terminal_input");
    expect(inputCalls()).toEqual([{ type: "terminal_input", data: "abc" }]);

    // Input arriving while first POST is in-flight waits until it completes.
    termState.onDataCb!("d");
    termState.onDataCb!("e");
    await vi.advanceTimersByTimeAsync(100);
    expect(inputCalls()).toHaveLength(1);

    resolveFirst();
    await Promise.resolve();
    await Promise.resolve();
    expect(inputCalls()).toEqual([
      { type: "terminal_input", data: "abc" },
      { type: "terminal_input", data: "de" },
    ]);

    // terminal_exited writes a status line to xterm.
    setEvents([{ event: { type: "terminal_exited", code: 0 }, atUnix: 1 }]);
    await Promise.resolve();
    expect(termState.lines.some((line) => line.includes("exited (code 0)"))).toBe(true);
  });

  it("debounces ResizeObserver terminal_resize and deduplicates unchanged dimensions", async () => {
    vi.useFakeTimers();
    installResizeObserverSpy();

    const send = vi.fn(() => Promise.resolve());
    const [state, setState] = createSignal<"connecting" | "live">("live");
    const relay: SessionRelay = {
      state,
      failure: () => null,
      events: () => [],
      send,
      dispose: vi.fn(),
    };

    render(() => <TerminalPanelImpl sessionId="s-1" relay={relay} onError={vi.fn()} />);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    expect(send).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenLastCalledWith({ type: "terminal_resize", cols: 80, rows: 24 });

    // Burst of ResizeObserver ticks with unchanged size sends nothing extra.
    termState.resizeObserverCb?.();
    termState.resizeObserverCb?.();
    await vi.advanceTimersByTimeAsync(150);
    expect(send).toHaveBeenCalledTimes(1);

    // Changing dimensions debounces to a single resize request.
    termState.cols = 100;
    termState.rows = 30;
    termState.resizeObserverCb?.();
    termState.cols = 120;
    termState.rows = 40;
    termState.resizeObserverCb?.();
    await vi.advanceTimersByTimeAsync(119);
    expect(send).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(send).toHaveBeenCalledTimes(2);
    expect(send).toHaveBeenLastCalledWith({ type: "terminal_resize", cols: 120, rows: 40 });

    // Reconnect re-announces the current size even when unchanged.
    setState("connecting");
    await Promise.resolve();
    setState("live");
    await Promise.resolve();
    expect(send).toHaveBeenCalledTimes(3);
  });
});
