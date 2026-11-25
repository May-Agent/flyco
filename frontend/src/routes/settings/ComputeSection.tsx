/**
 * Settings → Compute.
 *
 * Lists enrolled hosts and linked cloud accounts with an inline `+ Add compute`
 * subview inside the same settings pane.
 */
import { For, Show, createMemo, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import { A } from "@solidjs/router";
import { ArrowLeft, Plus } from "lucide-solid";
import ComputeCard from "../../components/ComputeCard";
import Flow from "../../components/flow/Flow";
import HostCard from "../../components/HostCard";
import ProblemNotice from "../../components/ProblemNotice";
import { useReadiness } from "../../components/Readiness";
import { listCloudUsage, listHosts, unlinkProvider, updateProvider } from "../../api/client";
import { EVERY_AGENT, linkedAgents } from "../../lib/flow";
import { cx } from "../../lib/cx";
import { setSpotPreference, spotPreference } from "../../lib/localPreferences";
import styles from "./Settings.module.css";

const ADD_COMPUTE = "/connect/compute?return=/settings/compute";

export default function ComputeSection() {
  const readiness = useReadiness();
  const [usage, { refetch: refetchUsage }] = createQuery(() => listCloudUsage());
  const [hosts, { refetch: refetchHosts }] = createQuery(() => listHosts());
  const [spot, setSpot] = createSignal(spotPreference());
  const [adding, setAdding] = createSignal(false);

  const machines = createMemo(() => new Set((hosts() ?? []).map((host) => host.id)));

  const clouds = createMemo(() => {
    const carded = machines();
    return readiness.compute().filter((account) => {
      const host = account.host_id ?? null;
      return host === null || !carded.has(host);
    });
  });

  async function unlink(id: string): Promise<void> {
    await unlinkProvider(id);
    await readiness.refresh();
    void refetchUsage();
  }

  async function updateQuota(id: string, limitMicros: number | null): Promise<void> {
    await updateProvider(
      id,
      limitMicros === null
        ? { clear_monthly_limit: true }
        : { monthly_limit: limitMicros, clear_monthly_limit: false },
    );
    await readiness.refresh();
  }

  async function hostChanged(): Promise<void> {
    await Promise.all([refetchHosts(), readiness.refresh()]);
  }

  function chooseSpot(next: boolean): void {
    setSpot(next);
    setSpotPreference(next);
  }

  function openAdd(event: MouseEvent): void {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.button !== 0) {
      return;
    }
    event.preventDefault();
    setAdding(true);
  }

  return (
    <section class={styles.section}>
      <Show
        when={adding()}
        fallback={
          <>
            <header class={styles.sectionHead}>
              <h2>Compute</h2>
              <Show when={readiness.compute().length > 0}>
                <A href={ADD_COMPUTE} class={styles.pill} onClick={openAdd}>
                  <Plus size={14} aria-hidden="true" />
                  Add compute
                </A>
              </Show>
            </header>

            <ProblemNotice error={readiness.error() ?? hosts.error ?? usage.error} />

            <Show
              when={readiness.compute().length > 0}
              fallback={
                <div class={styles.empty}>
                  <p class={styles.emptyLine}>No compute linked yet.</p>
                  <A href={ADD_COMPUTE} class={styles.pillPrimary} onClick={openAdd}>
                    <Plus size={14} aria-hidden="true" />
                    Add compute
                  </A>
                </div>
              }
            >
              <div class={styles.cards}>
                <For each={hosts() ?? []}>
                  {(host) => <HostCard host={host} onChanged={() => void hostChanged()} />}
                </For>
                <For each={clouds()}>
                  {(account) => (
                    <ComputeCard
                      account={account}
                      usage={usage()?.find((row) => row.account === account.id)}
                      spot={spot()}
                      onSpot={chooseSpot}
                      onUpdateQuota={(limit) => updateQuota(account.id, limit)}
                      onUnlink={() => unlink(account.id)}
                    />
                  )}
                </For>
              </div>
            </Show>
          </>
        }
      >
        <header class={styles.sectionHeadStack}>
          <button
            type="button"
            class={cx(styles.backLink, styles.backButton)}
            onClick={() => setAdding(false)}
          >
            <ArrowLeft size={14} aria-hidden="true" />
            Back to Compute
          </button>
        </header>
        <Flow
          embedded
          stages={["compute"]}
          agents={EVERY_AGENT}
          answers={{ agents: linkedAgents(readiness.harness()) }}
          onDone={() => {
            void Promise.all([readiness.refresh(), refetchHosts(), refetchUsage()]);
            setAdding(false);
          }}
          onLeave={() => setAdding(false)}
        />
      </Show>
    </section>
  );
}
