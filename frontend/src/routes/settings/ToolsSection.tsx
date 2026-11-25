/**
 * Settings → Tools.
 *
 * Grouped inset lists for MCP servers and Skills, with inline registry and
 * marketplace subviews directly inside the settings pane and a compact file
 * upload control instead of a permanent oversized dropzone.
 */
import { For, Match, Show, Switch, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { createQuery } from "../../lib/query";
import { LibraryBig, Plus, Upload } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import Skeleton from "../../components/Skeleton";
import Toggle from "../../components/Toggle";
import Modal from "../../components/Modal";
import McpServerForm from "./McpServerForm";
import { McpCatalogView } from "./McpCatalog";
import { SkillCatalogView } from "./SkillCatalog";
import {
  deleteMcpServer,
  deleteSkill,
  listMcpServers,
  listSkills,
  updateMcpServer,
  uploadSkill,
  type McpServerConfig,
  type McpServerView,
  type SkillView,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

const MCP_CATALOG = "/settings/tools/mcp-catalog";
const SKILL_CATALOG = "/settings/tools/skill-catalog";

function describeConfig(config: McpServerConfig): string {
  return config.transport === "stdio"
    ? `${config.command} ${config.args.join(" ")}`.trim()
    : config.url;
}

export default function ToolsSection() {
  const [subview, setSubview] = createSignal<"mcp" | "skills" | null>(null);

  return (
    <Switch
      fallback={
        <section class={styles.section}>
          <header class={styles.sectionHead}>
            <h2>Tools</h2>
          </header>
          <McpServers onOpenCatalog={() => setSubview("mcp")} />
          <Skills onOpenCatalog={() => setSubview("skills")} />
        </section>
      }
    >
      <Match when={subview() === "mcp"}>
        <McpCatalogView onDone={() => setSubview(null)} />
      </Match>
      <Match when={subview() === "skills"}>
        <SkillCatalogView onDone={() => setSubview(null)} />
      </Match>
    </Switch>
  );
}

function interceptClick(action: () => void) {
  return (event: MouseEvent): void => {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.button !== 0) {
      return;
    }
    event.preventDefault();
    action();
  };
}

/* ── MCP servers ──────────────────────────────────────────────────────── */

function McpServers(props: { onOpenCatalog: () => void }) {
  const [servers, { refetch }] = createQuery(listMcpServers);
  const listed = (): McpServerView[] => servers() ?? [];
  const [editing, setEditing] = createSignal<string | null>(null);
  const [adding, setAdding] = createSignal(false);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  async function setEnabled(server: McpServerView, enabled: boolean): Promise<void> {
    setBusy(server.id);
    setError(null);
    try {
      await updateMcpServer(server.id, { name: server.name, enabled, config: server.config });
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(null);
    }
  }

  async function remove(id: string): Promise<void> {
    setBusy(id);
    setError(null);
    try {
      await deleteMcpServer(id);
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(null);
    }
  }

  return (
    <div class={styles.group}>
      <div class={styles.groupHeader}>
        <p class={styles.groupLabel}>MCP servers</p>
        <Show when={servers.settled}>
          <div class={styles.actionsStart}>
            <A
              href={MCP_CATALOG}
              class={listed().length > 0 ? styles.pill : styles.pillPrimary}
              onClick={interceptClick(props.onOpenCatalog)}
            >
              <LibraryBig size={14} aria-hidden="true" />
              Add from catalog
            </A>
            <button type="button" class={styles.pill} onClick={() => setAdding(true)}>
              <Plus size={14} aria-hidden="true" />
              Add server
            </button>
          </div>
        </Show>
      </div>
      <ProblemNotice error={servers.error ?? error()} />

      <Show when={servers.settled} fallback={<Skeleton lines={3} />}>
        <Show when={listed().length > 0}>
          <div class={styles.insetList}>
            <For each={listed()}>
              {(server) => (
                <div class={styles.insetRow}>
                  <div class={styles.insetRowBar}>
                    <div class={styles.identity}>
                      <span class={styles.cardTitle}>{server.name}</span>
                      <span class={styles.mono}>{describeConfig(server.config)}</span>
                    </div>
                    <div class={styles.actions}>
                      <Toggle
                        label={`Give sessions ${server.name}`}
                        checked={server.enabled}
                        disabled={busy() === server.id}
                        onChange={(next) => void setEnabled(server, next)}
                      />
                      <button
                        type="button"
                        class={styles.pill}
                        onClick={() => setEditing(server.id)}
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        class={styles.pillDanger}
                        disabled={busy() === server.id}
                        onClick={() => void remove(server.id)}
                      >
                        Remove
                      </button>
                    </div>
                  </div>
                </div>
              )}
            </For>
          </div>
        </Show>
      </Show>

      <Show when={adding()}>
        <Modal title="Add MCP server" onClose={() => setAdding(false)}>
          <McpServerForm
            onSaved={() => {
              setAdding(false);
              void refetch();
            }}
            onCancel={() => setAdding(false)}
          />
        </Modal>
      </Show>
      <Show when={listed().find((server) => server.id === editing())}>
        {(server) => (
          <Modal title={`Edit ${server().name}`} onClose={() => setEditing(null)}>
            <McpServerForm
              server={server()}
              onSaved={() => {
                setEditing(null);
                void refetch();
              }}
              onCancel={() => setEditing(null)}
            />
          </Modal>
        )}
      </Show>
    </div>
  );
}

/* ── Skills ───────────────────────────────────────────────────────────── */

function Skills(props: { onOpenCatalog: () => void }) {
  const [skills, { refetch }] = createQuery(listSkills);
  const uploaded = (): SkillView[] => skills() ?? [];
  const [over, setOver] = createSignal(false);
  const [uploading, setUploading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let picker: HTMLInputElement | undefined;

  function directoryOf(fileName: string): string {
    return fileName.replace(/\.zip$/i, "");
  }

  async function upload(file: File): Promise<void> {
    if (!file.name.toLowerCase().endsWith(".zip")) {
      setError(new Error(`A skill is a .zip bundle; "${file.name}" is not one.`));
      return;
    }
    setUploading(true);
    setError(null);
    try {
      await uploadSkill(directoryOf(file.name), file);
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setUploading(false);
    }
  }

  async function remove(id: string): Promise<void> {
    setError(null);
    try {
      await deleteSkill(id);
      await refetch();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <div
      class={styles.group}
      onDragOver={(event) => {
        event.preventDefault();
        setOver(true);
      }}
      onDragLeave={() => setOver(false)}
      onDrop={(event) => {
        event.preventDefault();
        setOver(false);
        const file = event.dataTransfer?.files.item(0);
        if (file !== null && file !== undefined) {
          void upload(file);
        }
      }}
    >
      <div class={styles.groupHeader}>
        <p class={styles.groupLabel}>Skills</p>
        <div class={styles.actionsStart}>
          <A
            href={SKILL_CATALOG}
            class={uploaded().length > 0 ? styles.pill : styles.pillPrimary}
            onClick={interceptClick(props.onOpenCatalog)}
          >
            <LibraryBig size={14} aria-hidden="true" />
            Add from a marketplace
          </A>
          <input
            ref={picker}
            type="file"
            accept=".zip"
            class="visually-hidden"
            aria-label="Skill bundle"
            onChange={(event) => {
              const file = event.currentTarget.files?.item(0);
              if (file !== null && file !== undefined) {
                void upload(file);
              }
              event.currentTarget.value = "";
            }}
          />
          <button
            type="button"
            class={styles.pill}
            disabled={uploading()}
            onClick={() => picker?.click()}
          >
            <Upload size={14} aria-hidden="true" />
            {uploading() ? "Uploading…" : "Choose a file"}
          </button>
        </div>
      </div>

      <ProblemNotice error={skills.error ?? error()} />

      <Show when={uploaded().length > 0}>
        <div class={cx(styles.insetList, over() && styles.insetListDrop)}>
          <For each={uploaded()}>
            {(skill) => (
              <div class={styles.insetRow}>
                <div class={styles.insetRowBar}>
                  <div class={styles.identity}>
                    <span class={styles.cardTitle}>{skill.name}</span>
                    <span class={styles.cardMeta}>
                      Updated {formatDate(skill.uploaded_at_unix)}
                    </span>
                  </div>
                  <div class={styles.actions}>
                    <button
                      type="button"
                      class={styles.pillDanger}
                      onClick={() => void remove(skill.id)}
                    >
                      Remove
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
