/**
 * Read-only live Swarm panel for Antigravity sessions.
 *
 * Renders the latest swarm snapshot (goal, thread groups, member roster) and
 * the ordered `@` message board stream. Intervention stays on the session's
 * normal message composer below.
 */
import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { ChevronDown, ChevronRight, Users } from "lucide-solid";
import { cx } from "../lib/cx";
import {
  activeMemberCount,
  formatBoardTime,
  memberStatusLabel,
  memberStatusTone,
  segmentMentions,
  type SwarmState,
} from "../lib/swarm";
import styles from "./SwarmPanel.module.css";

export interface SwarmPanelProps {
  swarm: SwarmState;
}

export default function SwarmPanel(props: SwarmPanelProps) {
  const [expanded, setExpanded] = createSignal(true);
  let boardList: HTMLOListElement | undefined;
  let pinned = true;

  const activeCount = createMemo(() => activeMemberCount(props.swarm.members));
  const heading = () => props.swarm.title ?? "Swarm";

  function noteBoardScroll(): void {
    if (boardList === undefined) {
      return;
    }
    pinned = boardList.scrollHeight - boardList.scrollTop - boardList.clientHeight <= 48;
  }

  createEffect(() => {
    const count = props.swarm.messages.length;
    if (expanded() && count > 0 && boardList !== undefined && pinned) {
      boardList.scrollTop = boardList.scrollHeight;
    }
  });

  return (
    <section class={styles.panel} aria-label="Swarm">
      <button
        type="button"
        class={styles.header}
        aria-expanded={expanded()}
        onClick={() => setExpanded((was) => !was)}
      >
        <span class={styles.chevron} aria-hidden="true">
          <Show when={expanded()} fallback={<ChevronRight size={14} />}>
            <ChevronDown size={14} />
          </Show>
        </span>
        <Users size={14} class={cx(styles.icon)} aria-hidden="true" />
        <span class={styles.title}>{heading()}</span>
        <div class={styles.counts}>
          <Show when={props.swarm.members.length > 0}>
            <span class={styles.count}>
              <span class={styles.countNum}>{activeCount()}</span>
              <span class={styles.countSep}>/</span>
              <span class={styles.countNum}>{props.swarm.members.length}</span>
              <span class={styles.countLabel}>active</span>
            </span>
          </Show>
          <Show when={props.swarm.messages.length > 0}>
            <span class={styles.count}>
              <span class={styles.countNum}>{props.swarm.messages.length}</span>
              <span class={styles.countLabel}>
                {props.swarm.messages.length === 1 ? "post" : "posts"}
              </span>
            </span>
          </Show>
        </div>
      </button>

      <Show when={expanded()}>
        <div class={styles.body}>
          <Show when={props.swarm.goal}>
            {(goal) => (
              <div class={styles.goal}>
                <h3 class={styles.goalTitle}>{goal().title}</h3>
                <Show when={goal().line}>
                  {(line) => <p class={styles.goalLine}>{line()}</p>}
                </Show>
              </div>
            )}
          </Show>

          <Show when={props.swarm.threads.length > 0}>
            <div class={styles.section}>
              <h3 class={styles.sectionHeading}>Threads</h3>
              <ul class={styles.threadList}>
                <For each={props.swarm.threads}>
                  {(thread) => (
                    <li class={styles.threadCard}>
                      <div class={styles.threadHead}>
                        <span class={styles.threadTitle}>{thread.title}</span>
                        <Show when={thread.goal}>
                          {(goal) => <span class={styles.threadGoal}>{goal()}</span>}
                        </Show>
                      </div>
                      <Show when={(thread.tasks?.length ?? 0) > 0}>
                        <ul class={styles.threadTasks}>
                          <For each={thread.tasks ?? []}>
                            {(taskName) => (
                              <li class={styles.taskRow}>
                                <span class={styles.taskName}>{taskName}</span>
                                <Show when={props.swarm.tasks[taskName]}>
                                  {(summary) => (
                                    <span class={styles.taskSummary}>{summary()}</span>
                                  )}
                                </Show>
                              </li>
                            )}
                          </For>
                        </ul>
                      </Show>
                    </li>
                  )}
                </For>
              </ul>
            </div>
          </Show>

          <Show when={props.swarm.members.length > 0}>
            <div class={styles.section}>
              <h3 class={styles.sectionHeading}>Members</h3>
              <div class={styles.roster} role="table" aria-label="Swarm members">
                <div class={styles.rosterHead} role="row">
                  <span role="columnheader">Member</span>
                  <span role="columnheader">Role</span>
                  <span role="columnheader">Model</span>
                  <span role="columnheader">Workspace</span>
                  <span role="columnheader">State</span>
                </div>
                <For each={props.swarm.members}>
                  {(member) => {
                    const tone = () => memberStatusTone(member);
                    return (
                      <div
                        class={cx(styles.rosterRow, member.killed && styles.rosterRowKilled)}
                        role="row"
                      >
                        <span class={styles.memberCell} role="cell">
                          <span
                            class={styles.statusDot}
                            data-tone={tone()}
                            aria-hidden="true"
                          />
                          <span class={styles.memberName}>{member.name}</span>
                        </span>
                        <span class={styles.roleCell} role="cell">
                          {member.role ?? "—"}
                        </span>
                        <span
                          class={styles.modelCell}
                          role="cell"
                          title={member.model ?? undefined}
                        >
                          {member.model ?? "—"}
                        </span>
                        <span class={styles.workspaceCell} role="cell">
                          {member.workspace_mode ?? "—"}
                        </span>
                        <span
                          class={styles.stateCell}
                          data-tone={tone()}
                          role="cell"
                        >
                          {memberStatusLabel(member)}
                        </span>
                      </div>
                    );
                  }}
                </For>
              </div>
            </div>
          </Show>

          <Show when={props.swarm.messages.length > 0}>
            <div class={styles.section}>
              <h3 class={styles.sectionHeading}>Board</h3>
              <ol
                class={styles.board}
                ref={boardList}
                onScroll={noteBoardScroll}
                aria-label="Swarm message board"
              >
                <For each={props.swarm.messages}>
                  {(message) => {
                    const formattedTime = () => formatBoardTime(message.time);
                    return (
                      <li class={styles.post}>
                        <div class={styles.postMeta}>
                          <span class={styles.postSeq}>#{message.seq}</span>
                          <span class={styles.postAgent}>{message.agent}</span>
                          <Show when={message.kind}>
                            {(kind) => <span class={styles.postKind}>{kind()}</span>}
                          </Show>
                          <Show when={message.truncated === true}>
                            <span class={styles.postTruncated}>truncated</span>
                          </Show>
                          <Show when={formattedTime()}>
                            {(time) => <time class={styles.postTime}>{time()}</time>}
                          </Show>
                        </div>
                        <p class={styles.postBody}>
                          <For each={segmentMentions(message.text)}>
                            {(segment) =>
                              segment.kind === "mention" ? (
                                <span class={styles.mention}>{segment.text}</span>
                              ) : (
                                segment.text
                              )
                            }
                          </For>
                        </p>
                      </li>
                    );
                  }}
                </For>
              </ol>
            </div>
          </Show>
        </div>
      </Show>
    </section>
  );
}
