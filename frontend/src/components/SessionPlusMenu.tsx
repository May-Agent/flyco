/**
 * Consolidated `+` popover menu for the in-session composer.
 *
 * Keeps occasional session tools (Machine & Resize, Compute budget,
 * Repositories, Environment `.env`, Goal) one click away without crowding
 * the typing bar. Drill-in views stay inside the same popover.
 */
import { For, Show, createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";
import {
  ArrowLeft,
  ChevronRight,
  FolderGit2,
  Plus,
  Server,
  SlidersHorizontal,
  Target,
  Wallet,
} from "lucide-solid";
import Popover from "./Popover";
import MachinePanel from "./MachinePanel";
import SearchField from "./SearchField";
import RepoBranchPicker from "./RepoBranchPicker";
import ProblemNotice from "./ProblemNotice";
import { DEFAULT_BUDGET, MAX_BUDGET } from "./BudgetPicker";
import { listRepos, type MachineView, type SessionRepo } from "../api/client";
import { createQuery } from "../lib/query";
import { cx } from "../lib/cx";
import { machineChip } from "../lib/machines";
import composerStyles from "./Composer.module.css";
import budgetStyles from "./BudgetPicker.module.css";
import styles from "./SessionPlusMenu.module.css";

type Subview = "menu" | "machine" | "budget" | "repos" | "goal";

export interface SessionPlusMenuProps {
  sessionId: string;
  machine: MachineView | undefined;
  onMachineChanged: () => void;
  openMachineAt?: number | undefined;
  openResizeAt?: number | undefined;
  budgetSpentUsd: number | undefined;
  budgetLimitUsd: number | undefined;
  savingBudget: boolean;
  onSetBudget: (dollars: number) => void;
  repos: readonly SessionRepo[];
  onAddRepo?: ((selection: { repo: string; branch?: string }) => void) | undefined;
  addingRepo?: boolean | undefined;
  onOpenEnv: () => void;
  goalDescription?: string | undefined;
  onSetGoal?: ((condition: string) => void) | undefined;
}

export default function SessionPlusMenu(props: SessionPlusMenuProps) {
  const [initialView, setInitialView] = createSignal<Subview>("menu");

  createEffect(
    on(
      () => props.openMachineAt,
      (at) => {
        if (at !== undefined) {
          setInitialView("machine");
        }
      },
    ),
  );

  return (
    <>
      <Show when={props.machine}>
        {(view) => <span class="visually-hidden">{machineChip(view())}</span>}
      </Show>
      <Popover
        label="Session tools"
        align="start"
        side="top"
        openAt={props.openMachineAt}
        panelClass={styles.panel}
        trigger={(attrs) => (
          <button
            id={attrs.id}
            onClick={() => {
              setInitialView("menu");
              attrs.onClick();
            }}
            aria-expanded={attrs.expanded()}
            aria-haspopup="dialog"
            type="button"
            class={composerStyles.iconTrigger}
            title="Session tools"
            aria-label="Session tools"
          >
            <Plus size={16} aria-hidden="true" />
          </button>
        )}
      >
        {(close) => {
          const [view, setView] = createSignal<Subview>(initialView());
          onCleanup(() => setInitialView("menu"));

          // Sync programmatic resize requests while already open.
          createEffect(
            on(
              () => props.openResizeAt,
              (at) => {
                if (at !== undefined) {
                  setView("machine");
                }
              },
              { defer: true },
            ),
          );

          return (
            <div class={styles.container}>
              <Show when={view() === "menu"}>
                <ul class={styles.menuList} role="menu" aria-label="Session tools">
                  <Show when={props.onAddRepo !== undefined || props.repos.length > 0}>
                    <li>
                      <button
                        type="button"
                        role="menuitem"
                        class={styles.item}
                        onClick={() => setView("repos")}
                      >
                        <FolderGit2 size={15} aria-hidden="true" />
                        <span class={styles.itemLabel}>Repositories</span>
                        <span class={styles.itemMeta}>
                          {props.repos[0]?.slug.split("/").pop() ?? `${props.repos.length}`}
                          <Show when={props.repos.length > 1}>{` +${props.repos.length - 1}`}</Show>
                        </span>
                        <ChevronRight size={14} class={cx(styles.chevron)} aria-hidden="true" />
                      </button>
                    </li>
                  </Show>
                  <Show when={props.machine !== undefined}>
                    <li>
                      <button
                        type="button"
                        role="menuitem"
                        class={styles.item}
                        onClick={() => setView("machine")}
                      >
                        <Server size={15} aria-hidden="true" />
                        <span class={styles.itemLabel}>Machine</span>
                        <Show when={props.machine}>
                          {(m) => <span class={styles.itemMeta}>{machineChip(m())}</span>}
                        </Show>
                        <ChevronRight size={14} class={cx(styles.chevron)} aria-hidden="true" />
                      </button>
                    </li>
                  </Show>
                  <li>
                    <button
                      type="button"
                      role="menuitem"
                      class={styles.item}
                      onClick={() => setView("budget")}
                    >
                      <Wallet size={15} aria-hidden="true" />
                      <span class={styles.itemLabel}>Compute budget</span>
                      <span class={styles.itemMeta}>
                        ${(props.budgetSpentUsd ?? 0).toFixed(2)} / $
                        {(props.budgetLimitUsd ?? DEFAULT_BUDGET).toFixed(0)}
                      </span>
                      <ChevronRight size={14} class={cx(styles.chevron)} aria-hidden="true" />
                    </button>
                  </li>
                  <li>
                    <button
                      type="button"
                      role="menuitem"
                      class={styles.item}
                      onClick={() => {
                        close();
                        props.onOpenEnv();
                      }}
                    >
                      <SlidersHorizontal size={15} aria-hidden="true" />
                      <span class={styles.itemLabel}>Environment (.env)</span>
                    </button>
                  </li>
                  <Show when={props.onSetGoal !== undefined}>
                    <li>
                      <button
                        type="button"
                        role="menuitem"
                        class={styles.item}
                        onClick={() => setView("goal")}
                      >
                        <Target size={15} aria-hidden="true" />
                        <span class={styles.itemLabel}>Goal</span>
                        <ChevronRight size={14} class={cx(styles.chevron)} aria-hidden="true" />
                      </button>
                    </li>
                  </Show>
                </ul>
              </Show>

              <Show when={view() === "machine" && props.machine}>
                {(m) => (
                  <div class={styles.subview}>
                    <button
                      type="button"
                      class={styles.back}
                      onClick={() => setView("menu")}
                    >
                      <ArrowLeft size={14} aria-hidden="true" />
                      Back
                    </button>
                    <MachinePanel
                      sessionId={props.sessionId}
                      machine={m()}
                      openResize={props.openResizeAt}
                      onChanged={props.onMachineChanged}
                      embedded
                    />
                  </div>
                )}
              </Show>

              <Show when={view() === "budget"}>
                <BudgetSubview
                  spentUsd={props.budgetSpentUsd}
                  limitUsd={props.budgetLimitUsd}
                  saving={props.savingBudget}
                  onBack={() => setView("menu")}
                  onCommit={(dollars) => {
                    close();
                    if (dollars !== props.budgetLimitUsd) {
                      props.onSetBudget(dollars);
                    }
                  }}
                />
              </Show>

              <Show when={view() === "repos"}>
                <ReposSubview
                  repos={props.repos}
                  onAddRepo={props.onAddRepo}
                  adding={props.addingRepo}
                  onBack={() => setView("menu")}
                  onDone={close}
                />
              </Show>

              <Show when={view() === "goal" && props.onSetGoal}>
                {(onSetGoal) => (
                  <GoalSubview
                    onBack={() => setView("menu")}
                    onSubmit={(condition) => {
                      close();
                      onSetGoal()(condition);
                    }}
                  />
                )}
              </Show>
            </div>
          );
        }}
      </Popover>
    </>
  );
}

function BudgetSubview(props: {
  spentUsd: number | undefined;
  limitUsd: number | undefined;
  saving: boolean;
  onBack: () => void;
  onCommit: (dollars: number) => void;
}) {
  const floor = () => Math.floor(props.spentUsd ?? 0) + 1;
  const [draft, setDraft] = createSignal<number | undefined>(undefined);
  const amount = () => draft() ?? Math.max(floor(), Math.round(props.limitUsd ?? DEFAULT_BUDGET));
  const max = () => Math.max(MAX_BUDGET, amount(), floor());
  const travelled = () => {
    const span = max() - floor();
    if (span <= 0) return "0%";
    const clamped = Math.min(max(), Math.max(floor(), amount()));
    return `${((clamped - floor()) / span) * 100}%`;
  };

  return (
    <div class={styles.subview}>
      <button type="button" class={styles.back} onClick={props.onBack}>
        <ArrowLeft size={14} aria-hidden="true" />
        Back
      </button>
      <div class={budgetStyles.popover}>
        <div class={budgetStyles.head}>
          <span class={budgetStyles.amount}>${amount()}</span>
          <span class={budgetStyles.heading}>Compute budget</span>
        </div>
        <div class={budgetStyles.capsule}>
          <div class={budgetStyles.rail} style={{ "--travelled": travelled() }}>
            <span class={budgetStyles.fill} aria-hidden="true" />
            <span class={budgetStyles.thumb} aria-hidden="true" />
            <input
              class={budgetStyles.range}
              type="range"
              min={floor()}
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
          class={budgetStyles.commit}
          disabled={props.saving}
          onClick={() => props.onCommit(amount())}
        >
          {props.saving ? "Saving…" : `Set budget to $${amount()}`}
        </button>
      </div>
    </div>
  );
}

function ReposSubview(props: {
  repos: readonly SessionRepo[];
  onAddRepo: ((selection: { repo: string; branch?: string }) => void) | undefined;
  adding: boolean | undefined;
  onBack: () => void;
  onDone: () => void;
}) {
  const [query, setQuery] = createSignal("");
  const [debounced, setDebounced] = createSignal("");
  const [pickedBranch, setPickedBranch] = createSignal<Record<string, string | undefined>>({});

  createEffect(() => {
    const next = query();
    if (next === debounced()) return;
    const timer = setTimeout(() => setDebounced(next), 250);
    onCleanup(() => clearTimeout(timer));
  });

  const [results] = createQuery(
    () => (props.onAddRepo !== undefined ? debounced() : undefined),
    (needle: string) => listRepos(needle),
  );

  const attachable = createMemo(() => {
    const held = new Set(props.repos.map((entry) => entry.slug));
    return (results() ?? []).filter((candidate) => !held.has(candidate.slug));
  });

  return (
    <div class={styles.subview}>
      <button type="button" class={styles.back} onClick={props.onBack}>
        <ArrowLeft size={14} aria-hidden="true" />
        Back
      </button>
      <ul class={styles.repoList}>
        <For each={props.repos}>
          {(repo) => (
            <li class={styles.repoRow}>
              <FolderGit2 size={13} aria-hidden="true" />
              <span class={styles.repoSlug}>{repo.slug}</span>
              <Show when={repo.branch}>
                {(branch) => <span class={styles.repoBranch}>{branch()}</span>}
              </Show>
            </li>
          )}
        </For>
      </ul>
      <Show when={props.onAddRepo !== undefined}>
        <SearchField
          placeholder="Add repository…"
          aria-label="Search your repositories"
          value={query()}
          onInput={(event) => setQuery(event.currentTarget.value)}
        />
        <ul class={styles.repoResults}>
          <For each={attachable().slice(0, 8)}>
            {(candidate) => (
              <li class={styles.repoRow}>
                <span class={styles.repoSlug}>{candidate.slug}</span>
                <RepoBranchPicker
                  slug={candidate.slug}
                  branch={pickedBranch()[candidate.slug] ?? null}
                  onChoose={(branch) =>
                    setPickedBranch((held) => ({ ...held, [candidate.slug]: branch }))
                  }
                />
                <button
                  type="button"
                  class={styles.addBtn}
                  disabled={props.adding === true}
                  onClick={() => {
                    const branch = pickedBranch()[candidate.slug];
                    props.onAddRepo?.(
                      branch === undefined
                        ? { repo: candidate.slug }
                        : { repo: candidate.slug, branch },
                    );
                    props.onDone();
                  }}
                >
                  Add
                </button>
              </li>
            )}
          </For>
        </ul>
        <ProblemNotice error={results.error} />
      </Show>
    </div>
  );
}

function GoalSubview(props: { onBack: () => void; onSubmit: (condition: string) => void }) {
  const [condition, setCondition] = createSignal("");

  function submit(): void {
    const trimmed = condition().trim();
    if (trimmed !== "") {
      props.onSubmit(trimmed);
    }
  }

  return (
    <div class={styles.subview}>
      <button type="button" class={styles.back} onClick={props.onBack}>
        <ArrowLeft size={14} aria-hidden="true" />
        Back
      </button>
      <input
        class={styles.goalInput}
        value={condition()}
        onInput={(event) => setCondition(event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            submit();
          }
        }}
        placeholder="Keep working until…"
        aria-label="Goal condition"
        autofocus
      />
      <button
        type="button"
        class={budgetStyles.commit}
        disabled={condition().trim() === ""}
        onClick={submit}
      >
        Set goal
      </button>
    </div>
  );
}
