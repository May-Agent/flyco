/**
 * The one control that sets a session's compute budget.
 *
 * Controlled: the amount lives with the caller so the trigger and the
 * capsule slider stay in lockstep.
 */
import type { JSX } from "solid-js";
import { Show, createSignal, onCleanup } from "solid-js";
import Popover, { type TriggerAttrs } from "./Popover";
import styles from "./BudgetPicker.module.css";

/** What the budget slider spans, in whole dollars. */
export const MIN_BUDGET = 1;
export const MAX_BUDGET = 200;
export const DEFAULT_BUDGET = 10;

export interface BudgetPickerProps {
  /** The amount the panel shows, in whole dollars. */
  dollars: number;
  /** The new amount, on every move of the slider. */
  onChange: (dollars: number) => void;
  /** Lowest amount the slider offers. Defaults to {@link MIN_BUDGET}. */
  min?: number;
  /** Highest amount the slider offers. Defaults to {@link MAX_BUDGET}. */
  max?: number;
  /** What the panel is, for assistive technology. */
  label?: string;
  /** Which edge of the trigger the panel lines up with. */
  align?: "start" | "end" | undefined;
  /** The control that opens the panel. */
  trigger: (attrs: TriggerAttrs) => JSX.Element;
  /** What commits the change, for a caller whose change is a request. */
  action?: ((close: () => void) => JSX.Element) | undefined;
}

export default function BudgetPicker(props: BudgetPickerProps) {
  const min = () => props.min ?? MIN_BUDGET;
  const max = () => props.max ?? MAX_BUDGET;
  const travelled = () => {
    const span = max() - min();
    if (span <= 0) {
      return "0%";
    }
    const clamped = Math.min(max(), Math.max(min(), props.dollars));
    return `${((clamped - min()) / span) * 100}%`;
  };

  return (
    <Popover
      label={props.label ?? "Budget"}
      align={props.align}
      panelClass={styles.panel}
      trigger={props.trigger}
    >
      {(close) => (
        <div class={styles.popover}>
          <div class={styles.head}>
            <span class={styles.amount}>${props.dollars}</span>
            <span class={styles.heading}>Compute budget</span>
          </div>
          <div class={styles.capsule}>
            <div class={styles.rail} style={{ "--travelled": travelled() }}>
              <span class={styles.fill} aria-hidden="true" />
              <span class={styles.thumb} aria-hidden="true" />
              <input
                class={styles.range}
                type="range"
                min={min()}
                max={max()}
                step="1"
                value={props.dollars}
                aria-label="Session budget in dollars"
                onInput={(event) => props.onChange(Number(event.currentTarget.value))}
              />
            </div>
          </div>
          <Show when={props.action}>{(action) => action()(close)}</Show>
        </div>
      )}
    </Popover>
  );
}

export interface BudgetRaiseProps {
  /** What the session may spend now, in whole dollars. */
  limitUsd: number | undefined;
  /** What it has spent, in whole dollars. */
  spentUsd: number | undefined;
  /** Whether a change is in flight. */
  saving: boolean;
  /** Sets the new limit, in whole dollars. */
  onSet: (dollars: number) => void;
  /** The control that opens the panel. */
  trigger: (attrs: TriggerAttrs) => JSX.Element;
  /** What the panel is, for assistive technology. */
  label?: string;
  /** Which edge of the trigger the panel lines up with. */
  align?: "start" | "end" | undefined;
  /** Programmatic open timestamp from a parent menu. */
  openAt?: number | undefined;
}

export function BudgetRaise(props: BudgetRaiseProps) {
  const [draft, setDraft] = createSignal<number | undefined>(undefined);

  const floor = () => Math.floor(props.spentUsd ?? 0) + 1;
  const amount = () => draft() ?? Math.max(floor(), Math.round(props.limitUsd ?? DEFAULT_BUDGET));

  function commit(close: () => void): void {
    close();
    const next = amount();
    setDraft(undefined);
    if (next !== props.limitUsd) {
      props.onSet(next);
    }
  }

  return (
    <Popover
      label={props.label ?? "Session budget"}
      align={props.align}
      openAt={props.openAt}
      panelClass={styles.panel}
      trigger={props.trigger}
    >
      {(close) => {
        onCleanup(() => setDraft(undefined));
        const min = () => floor();
        const max = () => Math.max(MAX_BUDGET, amount(), floor());
        const travelled = () => {
          const span = max() - min();
          if (span <= 0) {
            return "0%";
          }
          const clamped = Math.min(max(), Math.max(min(), amount()));
          return `${((clamped - min()) / span) * 100}%`;
        };
        return (
          <div class={styles.popover}>
            <div class={styles.head}>
              <span class={styles.amount}>${amount()}</span>
              <span class={styles.heading}>Compute budget</span>
            </div>
            <div class={styles.capsule}>
              <div class={styles.rail} style={{ "--travelled": travelled() }}>
                <span class={styles.fill} aria-hidden="true" />
                <span class={styles.thumb} aria-hidden="true" />
                <input
                  class={styles.range}
                  type="range"
                  min={min()}
                  max={max()}
                  step="1"
                  value={amount()}
                  aria-label="Session budget in dollars"
                  onInput={(event) => setDraft(Number(event.currentTarget.value))}
                />
              </div>
            </div>
            <button
              type="button"
              class={styles.commit}
              disabled={props.saving}
              onClick={() => commit(close)}
            >
              {props.saving ? "Saving…" : `Set budget to $${amount()}`}
            </button>
          </div>
        );
      }}
    </Popover>
  );
}
