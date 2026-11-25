/**
 * Pending permission prompt that replaces the session composer while the agent
 * is waiting on a decision.
 *
 * Sitting in the composer's own slot means the decision is always anchored at
 * the foot of the viewport where the user expects to act next, rather than
 * buried as a chat card in the scrolling transcript. Once decided, the
 * composer returns to the slot.
 */
import { For, Match, Show, Switch } from "solid-js";
import { AlertTriangle } from "lucide-solid";
import { operation, type OperationDetail } from "../lib/approvals";
import { cx } from "../lib/cx";
import type { TranscriptItem } from "../lib/transcript";
import styles from "./ApprovalPrompt.module.css";

export type PendingApproval = Extract<TranscriptItem, { kind: "approval" }>;

export interface ApprovalPromptProps {
  /** The pending approval currently waiting for a decision. */
  approval: PendingApproval;
  /** Total number of pending approvals in the queue (including this one). */
  remaining: number;
  /** Whether a decision request is in flight. */
  deciding: boolean;
  /** Records the user's decision. */
  onDecide: (id: string, decision: "approved" | "denied") => void;
}

/**
 * Exactly what is being approved.
 *
 * A tool call's arguments are read out one per row — the name of the argument,
 * and the value as it was written — because the value is what the decision is
 * about, and finding a shell command inside pretty-printed JSON is work the
 * reader should not be doing (issue #136). A one-line value sits beside its
 * name; anything that runs over gets a block under it.
 */
function OperationDetailView(props: { detail: OperationDetail }) {
  return (
    <Switch>
      <Match when={props.detail.kind === "text" && props.detail}>
        {(detail) => <pre class={styles.detail}>{detail().text}</pre>}
      </Match>
      <Match when={props.detail.kind === "fields" && props.detail}>
        {(detail) => (
          <dl class={styles.fields}>
            <For each={detail().fields}>
              {(field) => (
                <div class={styles.field} data-block={field.block}>
                  <dt class={styles.fieldName}>{field.name}</dt>
                  <dd class={styles.fieldValue}>
                    <code>{field.value}</code>
                  </dd>
                </div>
              )}
            </For>
          </dl>
        )}
      </Match>
    </Switch>
  );
}

export default function ApprovalPrompt(props: ApprovalPromptProps) {
  const asked = () => operation(props.approval.payload);

  return (
    <section class={styles.prompt} aria-label="Approval">
      <div class={styles.header}>
        <AlertTriangle size={15} class={cx(styles.icon)} aria-hidden="true" />
        <p class={styles.title}>{asked().title}</p>
        <Show when={props.remaining > 1}>
          <span class={styles.queue}>1 / {props.remaining}</span>
        </Show>
      </div>
      <OperationDetailView detail={asked().detail} />
      <div class={styles.actions}>
        <button
          type="button"
          class={styles.deny}
          disabled={props.deciding}
          onClick={() => props.onDecide(props.approval.id, "denied")}
        >
          Deny
        </button>
        <button
          type="button"
          class={styles.approve}
          disabled={props.deciding}
          onClick={() => props.onDecide(props.approval.id, "approved")}
        >
          Approve
        </button>
      </div>
    </section>
  );
}
