/**
 * Settings → API keys.
 *
 * Compact inline creation bar above a grouped inset list of existing keys.
 */
import { For, Show, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import { Plus } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import CopyButton from "../../components/CopyButton";
import {
  createApiKey,
  listApiKeys,
  revokeApiKey,
  type CreatedApiKey,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import styles from "./Settings.module.css";

export default function ApiKeysSection() {
  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>API keys</h2>
      </header>

      <ApiKeys />
    </section>
  );
}

function ApiKeys() {
  const [keys, { refetch }] = createQuery(listApiKeys);
  const [label, setLabel] = createSignal("");
  const [creating, setCreating] = createSignal(false);
  const [created, setCreated] = createSignal<CreatedApiKey | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  async function create(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setCreating(true);
    setError(null);
    try {
      setCreated(await createApiKey(label()));
      setLabel("");
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(id: string): Promise<void> {
    setError(null);
    try {
      await revokeApiKey(id);
      await refetch();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <div class={styles.group}>
      <ProblemNotice error={keys.error ?? error()} />

      <form class={styles.inlineForm} onSubmit={(event) => void create(event)}>
        <label for="api-key-label" class="visually-hidden">
          New key label
        </label>
        <input
          id="api-key-label"
          value={label()}
          onInput={(event) => setLabel(event.currentTarget.value)}
          placeholder="Key name (e.g. laptop, CI)"
          required
        />
        <button type="submit" class={styles.pillPrimary} disabled={creating()}>
          <Plus size={14} aria-hidden="true" />
          {creating() ? "Creating…" : "Create key"}
        </button>
      </form>

      <Show when={created()}>
        {(key) => (
          <article class={styles.card}>
            <div class={styles.identity}>
              <span class={styles.cardTitle}>{key().label}</span>
              <span class={styles.cardMeta}>Shown once — copy before closing.</span>
            </div>
            <div class={styles.secret}>
              <span class={styles.secretValue}>{key().token}</span>
              <div class={styles.actions}>
                <CopyButton value={key().token} class={styles.pill} label="Copy key" />
                <button type="button" class={styles.pill} onClick={() => setCreated(null)}>
                  Done
                </button>
              </div>
            </div>
          </article>
        )}
      </Show>

      <Show when={(keys() ?? []).length > 0}>
        <div class={styles.insetList}>
          <For each={keys()}>
            {(key) => (
              <div class={styles.insetRow}>
                <div class={styles.insetRowBar}>
                  <div class={styles.identity}>
                    <span class={styles.cardTitle}>{key.label}</span>
                    <span class={styles.cardMeta}>
                      <span>Created {formatDate(key.created_at_unix)}</span>
                      <span>
                        {key.last_used_unix === null || key.last_used_unix === undefined
                          ? "Never used"
                          : `Used ${formatDate(key.last_used_unix)}`}
                      </span>
                    </span>
                  </div>
                  <div class={styles.actions}>
                    <button
                      type="button"
                      class={styles.pillDanger}
                      onClick={() => void revoke(key.id)}
                    >
                      Revoke
                    </button>
                  </div>
                </div>
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
