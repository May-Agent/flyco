/**
 * Settings → Agents.
 *
 * Grouped inset list of all supported agents, with inline sign-in / relink
 * subviews directly inside the settings pane rather than bouncing out to a
 * separate full-page wizard.
 */
import { For, Show, createSignal, onCleanup } from "solid-js";
import { A } from "@solidjs/router";
import { ArrowLeft } from "lucide-solid";
import { createQuery } from "../../lib/query";
import ConfirmDialog from "../../components/ConfirmDialog";
import Flow from "../../components/flow/Flow";
import Logomark, { HARNESS_MARK } from "../../components/Logomark";
import ProblemNotice from "../../components/ProblemNotice";
import HarnessUsage from "../../components/HarnessUsage";
import { useReadiness } from "../../components/Readiness";
import {
  listLlmUsage,
  unlinkHarnessAccount,
  type HarnessAccountView,
  type HarnessKind,
} from "../../api/client";
import { linkedAgents } from "../../lib/flow";
import { formatDate } from "../../lib/dates";
import { credentialExpiry } from "../../lib/expiry";
import { cx } from "../../lib/cx";
import { inUseRefusal, sessionsStillRunning, type InUseRefusal } from "../../lib/inUse";
import styles from "./Settings.module.css";

const HARNESSES: readonly { kind: HarnessKind; label: string }[] = [
  { kind: "claude_code", label: "Claude Code" },
  { kind: "codex", label: "Codex" },
  { kind: "devin", label: "Devin" },
  { kind: "antigravity", label: "Antigravity" },
];

const TICK_MS = 60_000;

export default function AgentsSection() {
  const readiness = useReadiness();
  const [now, setNow] = createSignal(Math.floor(Date.now() / 1000));
  const ticker = setInterval(() => setNow(Math.floor(Date.now() / 1000)), TICK_MS);
  onCleanup(() => clearInterval(ticker));
  const [usage, { refetch: refetchUsage }] = createQuery(listLlmUsage);
  const [connecting, setConnecting] = createSignal<HarnessKind | null>(null);
  const [actionError, setActionError] = createSignal<unknown>(null);
  const [unlinking, setUnlinking] = createSignal<HarnessAccountView | null>(null);
  const [refused, setRefused] = createSignal<InUseRefusal | null>(null);
  const [busy, setBusy] = createSignal(false);

  async function unlink(id: HarnessAccountView["id"]): Promise<void> {
    setActionError(null);
    setBusy(true);
    try {
      await unlinkHarnessAccount(id);
      setUnlinking(null);
      await readiness.refresh();
    } catch (err) {
      const inUse = inUseRefusal(err);
      if (inUse === null) {
        setActionError(err);
      } else {
        setRefused(inUse);
      }
    } finally {
      setBusy(false);
    }
  }

  function stopUnlinking(): void {
    setUnlinking(null);
    setRefused(null);
    setActionError(null);
  }

  const accountsFor = (kind: HarnessKind) =>
    readiness.harness().filter((account) => account.harness === kind);

  return (
    <section class={styles.section}>
      <Show
        when={connecting()}
        fallback={
          <>
            <header class={styles.sectionHead}>
              <h2>Agents</h2>
            </header>

            <ProblemNotice error={readiness.error() ?? usage.error} />

            <div class={styles.insetList}>
              <For each={HARNESSES}>
                {(harness) => {
                  const accounts = () => accountsFor(harness.kind);
                  return (
                    <Show
                      when={accounts().length > 0}
                      fallback={
                        <div class={styles.insetRow}>
                          <div class={styles.insetRowBar}>
                            <span class={styles.mark}>
                              <Logomark mark={HARNESS_MARK[harness.kind]} size={16} />
                            </span>
                            <div class={styles.identity}>
                              <div class={styles.identityInline}>
                                <span class={styles.cardTitle}>{harness.label}</span>
                                <span class={styles.status}>Not linked</span>
                              </div>
                            </div>
                            <div class={styles.actions}>
                              <button
                                type="button"
                                class={styles.pill}
                                onClick={() => setConnecting(harness.kind)}
                              >
                                Connect
                              </button>
                            </div>
                          </div>
                        </div>
                      }
                    >
                      <For each={accounts()}>
                        {(account) => {
                          const expiry = () => credentialExpiry(account.expires_at_unix, now());
                          return (
                            <div class={styles.insetRow}>
                              <div class={styles.insetRowBar}>
                                <span class={styles.mark}>
                                  <Logomark mark={HARNESS_MARK[harness.kind]} size={16} />
                                </span>
                                <div class={styles.identity}>
                                  <div class={styles.identityInline}>
                                    <span class={styles.cardTitle}>{harness.label}</span>
                                    <span
                                      class={cx(
                                        styles.status,
                                        expiry() === null || expiry()?.level === "fine"
                                          ? styles.statusOn
                                          : styles.statusWarn,
                                      )}
                                    >
                                      {account.label}
                                    </span>
                                  </div>
                                  <span class={styles.cardMeta}>
                                    <span>Linked {formatDate(account.linked_at_unix)}</span>
                                    <Show when={expiry()}>
                                      {(due) => (
                                        <span
                                          class={cx(
                                            due().level !== "fine" && styles.metaUrgent,
                                          )}
                                        >
                                          {due().sentence}
                                        </span>
                                      )}
                                    </Show>
                                  </span>
                                </div>
                                <div class={styles.actions}>
                                  <button
                                    type="button"
                                    class={
                                      expiry()?.level !== undefined && expiry()?.level !== "fine"
                                        ? styles.pillPrimary
                                        : styles.pill
                                    }
                                    onClick={() => setConnecting(harness.kind)}
                                  >
                                    Relink
                                  </button>
                                  <Show when={unlinking()?.id !== account.id}>
                                    <button
                                      type="button"
                                      class={styles.pillDanger}
                                      disabled={busy()}
                                      onClick={() => {
                                        setActionError(null);
                                        setRefused(null);
                                        setUnlinking(account);
                                      }}
                                    >
                                      Unlink
                                    </button>
                                  </Show>
                                </div>
                              </div>

                              <Show when={unlinking()?.id === account.id}>
                                <ConfirmDialog
                                  title={`Unlink ${account.label}?`}
                                  body={
                                    <Show
                                      when={refused()}
                                      fallback={
                                        <>
                                          Flyco forgets this credential and no new session can run{" "}
                                          {harness.label} on it. Signing in again links it back.
                                        </>
                                      }
                                    >
                                      {(inUse) => (
                                        <>
                                          {sessionsStillRunning(inUse())} They renew their{" "}
                                          {harness.label} token against this credential and would
                                          stop the moment it expired, so archive them first.
                                        </>
                                      )}
                                    </Show>
                                  }
                                  confirmLabel="Unlink"
                                  cancelLabel={refused() === null ? "Keep it" : "Close"}
                                  busy={busy()}
                                  {...(refused() === null
                                    ? { onConfirm: () => void unlink(account.id) }
                                    : {})}
                                  onCancel={stopUnlinking}
                                >
                                  <ProblemNotice error={actionError()} />
                                  <Show when={refused()}>
                                    <A href="/" class={styles.sessionsLink}>
                                      Go to Sessions
                                    </A>
                                  </Show>
                                </ConfirmDialog>
                              </Show>

                              <HarnessUsage
                                row={usage()?.find((row) => row.account === account.id)}
                                plan={account.usage}
                              />
                            </div>
                          );
                        }}
                      </For>
                    </Show>
                  );
                }}
              </For>
            </div>
          </>
        }
      >
        {(kind) => (
          <>
            <header class={styles.sectionHeadStack}>
              <button
                type="button"
                class={cx(styles.backLink, styles.backButton)}
                onClick={() => setConnecting(null)}
              >
                <ArrowLeft size={14} aria-hidden="true" />
                Back to Agents
              </button>
            </header>
            <Flow
              embedded
              stages={["agent"]}
              agents={[kind()]}
              answers={{ agents: linkedAgents(readiness.harness()) }}
              onDone={() => {
                void readiness.refresh();
                void refetchUsage();
                setConnecting(null);
              }}
              onLeave={() => setConnecting(null)}
            />
          </>
        )}
      </Show>
    </section>
  );
}
