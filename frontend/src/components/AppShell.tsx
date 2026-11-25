import {
  type JSX,
  Show,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
} from "solid-js";
import { createQuery } from "../lib/query";
import { A, Navigate, useLocation, useNavigate } from "@solidjs/router";
import {
  LogOut,
  Menu,
  PanelLeftClose,
  PanelLeftOpen,
  Settings as SettingsIcon,
  SquarePen,
} from "lucide-solid";
import Logomark, { FLYCO_MARK } from "./Logomark";
import Popover from "./Popover";
import ProblemNotice from "./ProblemNotice";
import { ReadinessProvider } from "./Readiness";
import SessionNav, { SessionsProvider } from "./SessionNav";
import SettingsModal from "./SettingsModal";
import { getMe } from "../api/client";
import { isSignedIn, onSessionChanged } from "../lib/session";
import { signOut } from "../lib/signOut";
import { cx } from "../lib/cx";
import styles from "./AppShell.module.css";

const PUBLIC_ROUTES = new Set(["/login", "/auth/complete"]);

const BARE_ROUTES = new Set([
  ...PUBLIC_ROUTES,
  "/welcome",
  "/connect/return",
  "/cli/authorize",
]);

export function initialsOf(login: string): string {
  const parts = login
    .split(/[^a-zA-Z0-9]+|(?=[A-Z])/u)
    .filter((part) => part.length > 0);
  const letters = parts.slice(0, 2).map((part) => part.charAt(0));
  return (
    letters.length > 0 ? letters.join("") : login.charAt(0)
  ).toUpperCase();
}

export default function AppShell(props: { children?: JSX.Element }) {
  const location = useLocation();
  const navigate = useNavigate();
  const [signedIn, setSignedIn] = createSignal(isSignedIn());
  const [railOpen, setRailOpen] = createSignal(false);
  const [railCollapsed, setRailCollapsed] = createSignal(false);
  const [settingsOpen, setSettingsOpen] = createSignal(false);
  const isBareRoute = createMemo(() => BARE_ROUTES.has(location.pathname));
  const isPublicRoute = createMemo(() => PUBLIC_ROUTES.has(location.pathname));
  const loginHref = createMemo(() => {
    const destination = `${location.pathname}${location.search}${location.hash}`;
    return `/login?returnTo=${encodeURIComponent(destination)}`;
  });

  createEffect(() => {
    location.pathname;
    setSignedIn(isSignedIn());
    setRailOpen(false);
  });

  const stopListening = onSessionChanged(() => setSignedIn(isSignedIn()));
  onCleanup(stopListening);

  function openSettings(): void {
    if (location.pathname.startsWith("/settings")) {
      return;
    }
    setSettingsOpen(true);
  }

  createEffect(() => {
    if (!signedIn() || isBareRoute()) {
      return;
    }
    function onKeyDown(event: KeyboardEvent): void {
      if (event.key === "," && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        if (location.pathname.startsWith("/settings")) {
          navigate("/");
        } else {
          setSettingsOpen((was) => !was);
        }
      }
    }
    document.addEventListener("keydown", onKeyDown);
    onCleanup(() => document.removeEventListener("keydown", onKeyDown));
  });

  return (
    <Show
      when={location.pathname !== "/login" || !signedIn()}
      fallback={<Navigate href="/" />}
    >
      <Show
        when={isPublicRoute() || signedIn()}
        fallback={<Navigate href={loginHref()} />}
      >
        <ReadinessProvider enabled={signedIn}>
          <SessionsProvider enabled={() => signedIn() && !isBareRoute()}>
            <div class={styles.shell}>
              <Show when={!isBareRoute()}>
                <button
                  type="button"
                  class={styles.railOpen}
                  aria-label="Open navigation"
                  aria-expanded={railOpen()}
                  onClick={() => setRailOpen(true)}
                >
                  <Menu size={16} aria-hidden="true" />
                </button>
                <Show when={railCollapsed()}>
                  <button
                    type="button"
                    class={styles.railExpandDesktop}
                    aria-label="Expand sidebar"
                    title="Expand sidebar"
                    onClick={() => setRailCollapsed(false)}
                  >
                    <PanelLeftOpen size={16} aria-hidden="true" />
                  </button>
                </Show>
                <Show when={railOpen()}>
                  <button
                    type="button"
                    class={styles.scrim}
                    aria-label="Close navigation"
                    onClick={() => setRailOpen(false)}
                  />
                </Show>
                <nav
                  class={cx(
                    styles.rail,
                    railOpen() && styles.railOpened,
                    railCollapsed() && styles.railCollapsed,
                  )}
                  aria-label="Primary"
                >
                  <div class={styles.brandRow}>
                    <A href="/" class={styles.brand}>
                      <Logomark mark={FLYCO_MARK} class={styles.brandMark} />
                      flyco
                    </A>
                    <button
                      type="button"
                      class={styles.railCollapseDesktop}
                      aria-label="Collapse sidebar"
                      title="Collapse sidebar"
                      onClick={() => setRailCollapsed(true)}
                    >
                      <PanelLeftClose size={16} aria-hidden="true" />
                    </button>
                    <button
                      type="button"
                      class={styles.railClose}
                      aria-label="Close navigation"
                      onClick={() => setRailOpen(false)}
                    >
                      <PanelLeftClose size={16} aria-hidden="true" />
                    </button>
                  </div>
                  <A href="/" end class={styles.newSession} onClick={() => setRailOpen(false)}>
                    <SquarePen size={15} aria-hidden="true" />
                    New session
                  </A>
                  <SessionNav onNavigate={() => setRailOpen(false)} />
                  <div class={styles.railFooter}>
                    <AccountMenu
                      onOpenSettings={openSettings}
                      onSignOut={() => signOut(navigate)}
                    />
                    <button
                      type="button"
                      class={styles.settingsButton}
                      aria-label="Settings"
                      title="Settings (⌘,)"
                      onClick={openSettings}
                    >
                      <SettingsIcon size={16} aria-hidden="true" />
                    </button>
                  </div>
                </nav>
                <Show when={settingsOpen()}>
                  <SettingsModal onClose={() => setSettingsOpen(false)} />
                </Show>
              </Show>
              <main class={cx(styles.main, railCollapsed() && styles.mainRailCollapsed)}>
                {props.children}
              </main>
            </div>
          </SessionsProvider>
        </ReadinessProvider>
      </Show>
    </Show>
  );
}

function AccountMenu(props: { onOpenSettings: () => void; onSignOut: () => void }) {
  const [me] = createQuery(getMe);
  const initials = createMemo(() => {
    const login = me()?.login;
    return login === undefined ? "" : initialsOf(login);
  });

  return (
    <Popover
      label="Account"
      align="start"
      side="top"
      anchorClass={styles.accountAnchor}
      panelClass={styles.menu}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={styles.account}
          aria-label={
            me() === undefined ? "Account" : `Account: ${me()?.login ?? ""}`
          }
        >
          <span class={styles.avatar} aria-hidden="true">
            <Show
              when={initials()}
              fallback={<span class={styles.avatarBlank} />}
            >
              {initials()}
            </Show>
          </span>
          <span class={styles.accountName}>{me()?.login ?? "Account"}</span>
        </button>
      )}
    >
      {(close) => (
        <>
          <ProblemNotice error={me.error} />
          <Show when={me()}>
            {(user) => (
              <p class={styles.menuIdentity}>
                Signed in as <strong>{user().login}</strong>
              </p>
            )}
          </Show>
          <button
            type="button"
            class={styles.menuLink}
            onClick={() => {
              close();
              props.onOpenSettings();
            }}
          >
            <SettingsIcon size={14} aria-hidden="true" />
            Settings
          </button>
          <button
            type="button"
            class={styles.menuAction}
            onClick={() => {
              close();
              props.onSignOut();
            }}
          >
            <LogOut size={14} aria-hidden="true" />
            Sign out
          </button>
        </>
      )}
    </Popover>
  );
}
