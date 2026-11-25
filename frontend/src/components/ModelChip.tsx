/**
 * Which model the agent runs, and how hard it thinks.
 *
 * One control in both composers: a quiet inline trigger showing the model's
 * short name and its effort level in separate spans, opening onto a centred
 * effort readout, a model switcher link, and a capsule detent slider.
 */
import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { Check, ChevronDown, ChevronRight } from "lucide-solid";
import Popover from "./Popover";
import SearchField from "./SearchField";
import Detents from "./Detents";
import type { ModelChoice, ModelOption } from "../api/client";
import { cx } from "../lib/cx";
import {
  choiceLabel,
  effortLabel,
  modelLabel,
  openingEffort,
  shortName,
} from "../lib/models";
import composer from "./Composer.module.css";
import styles from "./ModelChip.module.css";

const SEARCH_WORTH_IT = 8;

/**
 * Strips redundant model-name prefixes and generic filler ("a model on X.")
 * so rows only carry a second line when it states real information.
 */
function cleanModelDescription(option: ModelOption): string | null {
  let desc = option.description.trim();
  if (desc === "") {
    return null;
  }
  const clauses = desc.split(" · ");
  if (clauses.length > 1) {
    desc = clauses.slice(1).join(" — ").trim();
  }
  if (/^a model on [^,.]+\.?$/i.test(desc)) {
    return null;
  }
  return desc === "" ? null : desc;
}

export interface ModelChipProps {
  /** The models the agent offers. */
  models: readonly ModelOption[];
  /** What is chosen now. */
  choice: ModelChoice;
  /** The new choice, on a change of the model or the effort. */
  onChoose: (choice: ModelChoice) => void;
  /** Whether a change is still on its way to the agent. */
  saving?: boolean | undefined;
  /** Which edge of the chip the panel lines up with. */
  align?: "start" | "end" | undefined;
}

export default function ModelChip(props: ModelChipProps) {
  return (
    <Popover
      label="Model and effort"
      align={props.align}
      panelClass={styles.panel}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          aria-label={choiceLabel(props.models, props.choice)}
          type="button"
          class={cx(styles.trigger, props.saving === true && styles.saving)}
          title="Model"
        >
          <span class={styles.triggerModel}>{modelLabel(props.models, props.choice)}</span>
          <Show when={props.choice.effort}>
            {(effort) => <span class={styles.triggerEffort}>{effortLabel(effort())}</span>}
          </Show>
          <ChevronDown size={13} class={cx(styles.triggerChevron)} aria-hidden="true" />
        </button>
      )}
    >
      {(close) => {
        const [query, setQuery] = createSignal("");
        const [effortFor, setEffortFor] = createSignal<ModelOption | null>(
          props.models.find(
            (option) => option.id === props.choice.model && (option.efforts?.length ?? 0) > 0,
          ) ?? null,
        );

        const stops = createMemo<readonly string[]>(() => effortFor()?.efforts ?? []);

        const level = createMemo(() => {
          const option = effortFor();
          if (option === null) {
            return undefined;
          }
          const chosen = props.choice.model === option.id ? props.choice.effort : undefined;
          return chosen ?? openingEffort(option);
        });

        const position = createMemo(() => {
          const chosen = level();
          return chosen === undefined ? 0 : Math.max(0, stops().indexOf(chosen));
        });

        const reading = createMemo(() => {
          const chosen = level();
          return chosen === undefined ? "" : effortLabel(chosen);
        });

        const modelName = createMemo(() => {
          const option = effortFor();
          return option === null ? "" : shortName(option);
        });

        const filtered = createMemo<readonly ModelOption[]>(() => {
          const words = query().trim().toLowerCase().split(/\s+/).filter(Boolean);
          if (words.length === 0) {
            return props.models;
          }
          return props.models.filter((option) => {
            const text = `${option.label} ${option.id} ${option.description}`.toLowerCase();
            return words.every((word) => text.includes(word));
          });
        });

        createEffect(() => {
          if (effortFor() !== null && stops().length === 0) {
            setEffortFor(null);
          }
        });

        function pick(option: ModelOption): void {
          const opening = openingEffort(option);
          if (option.id !== props.choice.model) {
            props.onChoose(
              opening === undefined
                ? { model: option.id }
                : { model: option.id, effort: opening },
            );
          }
          if (option.efforts.length > 0) {
            setEffortFor(option);
          } else {
            close();
          }
        }

        function move(next: number): void {
          const stop = stops()[next];
          const option = effortFor();
          if (stop === undefined || option === null) {
            return;
          }
          props.onChoose({ model: option.id, effort: stop });
        }

        return (
          <Show
            when={effortFor() !== null && stops().length > 0}
            fallback={
              <div
                class={composer.popover}
                tabindex="-1"
                ref={(el) =>
                  queueMicrotask(() =>
                    (el.querySelector<HTMLElement>("input[type=search]") ?? el).focus({
                      preventScroll: true,
                    }),
                  )
                }
              >
                <Show when={props.models.length > SEARCH_WORTH_IT}>
                  <SearchField
                    placeholder="Search models"
                    aria-label="Search models"
                    value={query()}
                    onInput={(event) => setQuery(event.currentTarget.value)}
                  />
                </Show>
                <ul class={composer.optionsFill} role="listbox" aria-label="Model">
                  <For each={filtered()}>
                    {(option) => {
                      const detail = () => cleanModelDescription(option);
                      return (
                        <li>
                          <button
                            type="button"
                            role="option"
                            aria-selected={option.id === props.choice.model}
                            class={cx(
                              composer.option,
                              styles.model,
                              option.id === props.choice.model && composer.optionChosen,
                            )}
                            onClick={() => pick(option)}
                          >
                            <span class={styles.modelText}>
                              <span class={styles.modelName}>{option.label}</span>
                              <Show when={detail()}>
                                {(line) => <span class={styles.modelDescription}>{line()}</span>}
                              </Show>
                            </span>
                            <Show when={option.id === props.choice.model}>
                              <Check size={14} class={cx(styles.check)} aria-hidden="true" />
                            </Show>
                          </button>
                        </li>
                      );
                    }}
                  </For>
                </ul>
                <Show when={query().trim() !== "" && filtered().length === 0}>
                  <p class={composer.note}>No models match “{query().trim()}”.</p>
                </Show>
              </div>
            }
          >
            <div
              class={styles.effortPopover}
              tabindex="-1"
              ref={(el) =>
                queueMicrotask(() =>
                  (el.querySelector<HTMLElement>("input[type=range]") ?? el).focus({
                    preventScroll: true,
                  }),
                )
              }
            >
              <div class={styles.head}>
                <p class={styles.effortReading}>{reading()}</p>
                <button
                  type="button"
                  class={styles.models}
                  aria-label="Choose a model"
                  onClick={() => setEffortFor(null)}
                >
                  {modelName()}
                  <ChevronRight size={14} aria-hidden="true" />
                </button>
              </div>
              <div class={styles.control}>
                <Detents
                  count={stops().length}
                  position={position()}
                  ariaLabel="Effort"
                  ariaValueText={reading()}
                  onMove={move}
                />
              </div>
            </div>
          </Show>
        );
      }}
    </Popover>
  );
}
