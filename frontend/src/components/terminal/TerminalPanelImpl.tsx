import { createEffect, getOwner, onCleanup, onMount, runWithOwner } from "solid-js";
import type { Terminal as XTerm } from "@xterm/xterm";
import type { SessionRelay } from "../../api/relay";
import styles from "./TerminalPanel.module.css";

export interface TerminalPanelImplProps {
  sessionId: string;
  relay: SessionRelay;
  /** Where a refused keystroke or resize is reported; the page owns the banner. */
  onError: (failure: unknown) => void;
}

/**
 * The real terminal pane: an xterm.js view attached to the session's
 * `terminal_output` relay events, sending keystrokes back as
 * `terminal_input` commands. Only reachable through `TerminalPanel`'s
 * `lazy()` seam, so this module (and xterm's CSS) is never fetched until a
 * user opens the pane.
 *
 * The pane is fitted to the space it has, and the daemon is told the size
 * (`terminal_resize`) so the PTY wraps where the view does: on open, on
 * every resize of the pane, and again whenever the relay comes back live,
 * because a daemon that reconnected opened its PTY at a default size.
 */
export default function TerminalPanelImpl(props: TerminalPanelImplProps) {
  let container: HTMLDivElement | undefined;
  const owner = getOwner();
  let unmounted = false;
  onCleanup(() => {
    unmounted = true;
  });

  onMount(async () => {
    const [{ Terminal }, { FitAddon }] = await Promise.all([
      import("@xterm/xterm"),
      import("@xterm/addon-fit"),
      import("@xterm/xterm/css/xterm.css"),
    ]);
    if (unmounted || container === undefined || owner === null) {
      return;
    }

    // Everything below resumed after an `await`; Solid only threads the
    // reactive owner through synchronous continuations, so `onCleanup` and
    // `createEffect` are wrapped in `runWithOwner` against the owner
    // captured before the await, rather than the (by-now-detached) implicit
    // one.
    runWithOwner(owner, () => {
      // xterm paints its own ground, so it is told the page's terminal
      // colour rather than left to its stock black inside a not-black box.
      const ground = getComputedStyle(document.documentElement);
      const term: XTerm = new Terminal({
        convertEol: true,
        fontFamily: ground.getPropertyValue("--font-mono").trim(),
        fontSize: 13,
        theme: {
          background: ground.getPropertyValue("--color-terminal-bg").trim(),
          foreground: "#e8e6e0",
          cursor: "#e8e6e0",
          selectionBackground: "rgba(232, 230, 224, 0.25)",
        },
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(container);
      // The pane only mounts while the machine is connected — the drawer
      // shows its own notice otherwise — so the greeting is always this.
      term.writeln(`Connected to ${props.sessionId}'s fish shell.`);

      let pendingInput = "";
      let inputTimer: ReturnType<typeof setTimeout> | undefined;
      let inputInFlight = false;

      async function flushInput(): Promise<void> {
        if (unmounted || inputInFlight || pendingInput === "") {
          return;
        }
        if (props.relay.state() !== "live") {
          pendingInput = "";
          return;
        }
        inputInFlight = true;
        try {
          while (!unmounted && pendingInput !== "" && props.relay.state() === "live") {
            const batch = pendingInput;
            pendingInput = "";
            await props.relay.send({ type: "terminal_input", data: batch });
          }
        } catch (error) {
          if (!unmounted) {
            props.onError(error);
          }
        } finally {
          inputInFlight = false;
        }
      }

      const inputSubscription = term.onData((data) => {
        if (props.relay.state() !== "live") {
          return;
        }
        pendingInput += data;
        if (!inputInFlight && inputTimer === undefined) {
          inputTimer = setTimeout(() => {
            inputTimer = undefined;
            void flushInput();
          }, 40);
        }
      });

      let lastCols: number | null = null;
      let lastRows: number | null = null;
      let resizeTimer: ReturnType<typeof setTimeout> | undefined;

      function tellSize(force = false): void {
        if (unmounted || props.relay.state() !== "live") {
          return;
        }
        const cols = term.cols;
        const rows = term.rows;
        if (!force && cols === lastCols && rows === lastRows) {
          return;
        }
        lastCols = cols;
        lastRows = rows;
        void props.relay
          .send({ type: "terminal_resize", cols, rows })
          .catch((error) => {
            if (!unmounted) {
              props.onError(error);
            }
          });
      }
      const resizeSubscription = term.onResize(() => tellSize(false));
      fit.fit();
      const observer = new ResizeObserver(() => {
        if (resizeTimer !== undefined) {
          clearTimeout(resizeTimer);
        }
        resizeTimer = setTimeout(() => {
          resizeTimer = undefined;
          if (!unmounted) {
            fit.fit();
            tellSize(false);
          }
        }, 120);
      });
      observer.observe(container);
      createEffect(() => {
        if (props.relay.state() === "live") {
          tellSize(true);
        }
      });

      onCleanup(() => {
        if (inputTimer !== undefined) {
          clearTimeout(inputTimer);
          inputTimer = undefined;
        }
        if (resizeTimer !== undefined) {
          clearTimeout(resizeTimer);
          resizeTimer = undefined;
        }
        pendingInput = "";
        observer.disconnect();
        resizeSubscription.dispose();
        inputSubscription.dispose();
        term.dispose();
      });

      // Writes every not-yet-seen `terminal_output` / `terminal_exited` event.
      // `processed` tracks how far this has read so a re-run — the relay's
      // `events` signal changes on every new frame, not just terminal ones —
      // never rewrites history the terminal has already shown.
      let processed = 0;
      createEffect(() => {
        const events = props.relay.events();
        for (let i = processed; i < events.length; i += 1) {
          const entry = events[i];
          if (entry === undefined) {
            continue;
          }
          if (entry.event.type === "terminal_output") {
            term.write(entry.event.data);
          } else if (entry.event.type === "terminal_exited") {
            const suffix =
              entry.event.code === null ? "" : ` (code ${entry.event.code})`;
            term.writeln(`\r\n[Terminal process exited${suffix}]`);
          }
        }
        processed = events.length;
      });
    });
  });

  return <div class={styles.terminal} ref={container} />;
}
