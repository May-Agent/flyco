/**
 * The session switcher that lives in the rail (docs/ux.md §3).
 *
 * The list belongs beside the work rather than on a page of its own: the
 * thing a person does most often is move between running sessions, and a
 * layout that makes them go home first puts a navigation step in front of
 * every one of those moves.
 *
 * Grouped by repository, the way the official apps group by project,
 * because that is how a person remembers a session: "the helios one", not
 * "the idle one". Within a repository the newest is first. What a session
 * is doing is a dot, and only when it matters — a session that is working,
 * building its machine, or failed carries one; a session at rest carries
 * nothing, and neither does one whose agent has answered and is waiting
 * for a reply, because after a day's work that is every session there is.
 * A rail of forty coloured dots is a rail that says nothing.
 */
import {
  For,
  Show,
  createContext,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  untrack,
  useContext,
  type JSX,
} from "solid-js";
import { A, useMatch } from "@solidjs/router";
import { ChevronRight, Folder, FolderOpen } from "lucide-solid";
import { createQuery, type Query } from "../lib/query";
import { listSessions, type SessionSummary } from "../api/client";
import { cx } from "../lib/cx";
import Skeleton from "./Skeleton";
import { deriveStatus, isArchived, type StatusView } from "../lib/status";
import styles from "./SessionNav.module.css";

export interface SessionsStore {
  sessions: Query<SessionSummary[]>;
  refresh: () => Promise<SessionSummary[] | null | undefined>;
  syncSession: (summary: SessionSummary) => void;
}

const SessionsContext = createContext<SessionsStore>();

function mergeSessionList(
  prev: SessionSummary[] | undefined,
  updates: ReadonlyMap<string, SessionSummary>,
): SessionSummary[] {
  const base = prev ? [...prev] : [];
  for (const next of updates.values()) {
    const idx = base.findIndex((item) => item.id === next.id);
    if (idx === -1) {
      base.unshift(next);
    } else {
      base[idx] = { ...base[idx], ...next };
    }
  }
  return base;
}

/**
 * Shared session list owned by the app shell so `SessionNav`, `Home`, and
 * `SessionDetail` read one query and stay in sync on create/rename/archive/state changes.
 */
export function SessionsProvider(props: {
  enabled: () => boolean;
  children: JSX.Element;
}) {
  const when = () => (props.enabled() ? true : undefined);
  const [sessions, { refetch, mutate }] = createQuery(when, listSessions);
  const pendingUpdates = new Map<string, SessionSummary>();

  createEffect(() => {
    const current = sessions();
    if (current === undefined || pendingUpdates.size === 0) {
      return;
    }
    const merged = mergeSessionList(current, pendingUpdates);
    pendingUpdates.clear();
    mutate(merged);
  });

  function syncSession(summary: SessionSummary): void {
    untrack(() => {
      if (sessions() === undefined) {
        pendingUpdates.set(summary.id, summary);
        return;
      }
      mutate((prev) => mergeSessionList(prev, new Map([[summary.id, summary]])));
    });
  }

  return (
    <SessionsContext.Provider
      value={{
        sessions,
        refresh: async () => refetch(),
        syncSession,
      }}
    >
      {props.children}
    </SessionsContext.Provider>
  );
}

/**
 * Reads the shared sessions store, falling back to an isolated query when a
 * component is rendered outside `<SessionsProvider>` (e.g. standalone unit tests).
 */
export function useSessions(): SessionsStore {
  const shared = useContext(SessionsContext);
  if (shared !== undefined) {
    return shared;
  }
  const [sessions, { refetch, mutate }] = createQuery(listSessions);
  return {
    sessions,
    refresh: async () => refetch(),
    syncSession(summary: SessionSummary) {
      untrack(() => {
        mutate((prev) => mergeSessionList(prev, new Map([[summary.id, summary]])));
      });
    },
  };
}

/**
 * How often the rail re-reads the clock.
 *
 * Slower than the session page's own tick: no row prints a duration, so
 * this exists only so a session that changes state moves its dot without a
 * reload.
 */
const TICK_MS = 5000;

/** One repository's sessions, newest first. */
interface RepoGroup {
  repo: string;
  rows: { session: SessionSummary; status: StatusView }[];
}

/**
 * Divides sessions by repository, ordered by each repository's most recent
 * activity, so the project being worked on today is at the top.
 */
export function groupByRepo(
  rows: readonly { session: SessionSummary; status: StatusView }[],
): RepoGroup[] {
  const byRepo = new Map<string, RepoGroup>();
  for (const row of rows) {
    // The group is the primary repository — `repos[0]` — because that is
    // what the session's own header names, and a rail that grouped by
    // anything else would disagree with the page it opens.
    const repo = row.session.repos[0]?.slug ?? "";
    const held = byRepo.get(repo);
    if (held === undefined) {
      byRepo.set(repo, { repo, rows: [row] });
    } else {
      held.rows.push(row);
    }
  }
  const latest = (group: RepoGroup) =>
    Math.max(...group.rows.map((row) => row.session.last_active_unix));
  return [...byRepo.values()]
    .map((group) => ({
      ...group,
      rows: [...group.rows].sort(
        (left, right) => right.session.last_active_unix - left.session.last_active_unix,
      ),
    }))
    .sort((left, right) => latest(right) - latest(left));
}

export interface SessionNavProps {
  /** Called when a session is chosen, so a mobile drawer can close. */
  onNavigate?: (() => void) | undefined;
}

export default function SessionNav(props: SessionNavProps) {
  const { sessions } = useSessions();
  const [query, setQuery] = createSignal("");
  const [showArchived, setShowArchived] = createSignal(false);
  const [collapsedRepos, setCollapsedRepos] = createSignal<ReadonlySet<string>>(new Set());
  const current = useMatch(() => "/sessions/:id");

  function toggleRepoGroup(repo: string): void {
    const next = new Set(collapsedRepos());
    if (next.has(repo)) {
      next.delete(repo);
    } else {
      next.add(repo);
    }
    setCollapsedRepos(next);
  }

  const [now, setNow] = createSignal(Date.now());
  const ticker = setInterval(() => setNow(Date.now()), TICK_MS);
  onCleanup(() => clearInterval(ticker));

  const matching = createMemo(() => {
    const needle = query().trim().toLowerCase();
    return (sessions() ?? [])
      .filter(
        (session) =>
          needle === "" ||
          session.title.toLowerCase().includes(needle) ||
          session.repos.some((repo) => repo.slug.toLowerCase().includes(needle)),
      )
      .map((session) => ({
        session,
        status: deriveStatus(session, now()),
      }));
  });

  const visible = createMemo(() =>
    matching().filter(({ status }) => isArchived(status.status) === showArchived()),
  );

  const groups = createMemo(() => groupByRepo(visible()));

  const archivedCount = createMemo(
    () => matching().filter(({ status }) => isArchived(status.status)).length,
  );

  return (
    <div class={styles.nav}>
      <input
        class={styles.search}
        type="search"
        placeholder="Search"
        aria-label="Search sessions by title or repository"
        value={query()}
        onInput={(event) => setQuery(event.currentTarget.value)}
      />

      <div class={styles.scroller}>
        <Show
          when={visible().length > 0}
          fallback={
            /*
              Nothing is said about an empty list until the list has been
              read: the rail used to render "No sessions yet" in the
              moment before the first response and correct itself a frame
              later, which is the flash a person sees on every load.
            */
            <Show when={sessions.settled} fallback={<Skeleton lines={6} />}>
              <p class={styles.empty}>
                {showArchived() ? "Nothing archived." : "No sessions yet."}
              </p>
            </Show>
          }
        >
          <For each={groups()}>
            {(group) => {
              const folded = () =>
                query().trim() === "" && collapsedRepos().has(group.repo);
              const folderLabel = () => {
                const parts = group.repo.split("/");
                return parts[parts.length - 1] || group.repo || "Workspace";
              };
              return (
                <div class={styles.group}>
                  <button
                    type="button"
                    class={styles.groupToggle}
                    aria-expanded={!folded()}
                    title={group.repo}
                    onClick={() => toggleRepoGroup(group.repo)}
                  >
                    <Show
                      when={!folded()}
                      fallback={<Folder size={14} class={cx(styles.folderIcon)} aria-hidden="true" />}
                    >
                      <FolderOpen size={14} class={cx(styles.folderIcon)} aria-hidden="true" />
                    </Show>
                    <span class={styles.groupLabel}>{folderLabel()}</span>
                    <ChevronRight
                      size={12}
                      class={cx(styles.groupChevron, !folded() && styles.groupChevronOpen)}
                      aria-hidden="true"
                    />
                  </button>
                  <Show when={!folded()}>
                    <ul class={styles.list}>
                      <For each={group.rows}>
                        {({ session, status }) => (
                          <li>
                            <A
                              href={`/sessions/${session.id}`}
                              class={cx(
                                styles.row,
                                current()?.params.id === session.id && styles.rowCurrent,
                              )}
                              onClick={() => props.onNavigate?.()}
                              title={session.title}
                            >
                              <span class={styles.title}>{session.title}</span>
                              <span
                                class={styles.dot}
                                data-tone={status.tone}
                                aria-hidden="true"
                              />
                            </A>
                          </li>
                        )}
                      </For>
                    </ul>
                  </Show>
                </div>
              );
            }}
          </For>
        </Show>
      </div>

      <Show when={archivedCount() > 0 || showArchived()}>
        <button
          type="button"
          class={styles.archivedToggle}
          aria-pressed={showArchived()}
          onClick={() => setShowArchived((was) => !was)}
        >
          <Show
            when={showArchived()}
            fallback={
              <>
                <span>Archived</span>
                <span class={styles.archivedBadge}>{archivedCount()}</span>
              </>
            }
          >
            Back to sessions
          </Show>
        </button>
      </Show>
    </div>
  );
}
