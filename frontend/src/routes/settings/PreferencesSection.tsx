/**
 * Settings → Preferences.
 *
 * Grouped inset rows for appearance and browser notifications.
 */
import { For, Show, createSignal, onMount, type JSX } from "solid-js";
import { createQuery } from "../../lib/query";
import { BellRing, Monitor, Moon, Sun } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import { getVapidPublicKey } from "../../api/client";
import {
  getPushSubscription,
  isPushSupported,
  subscribeToPush,
  unsubscribeFromPush,
} from "../../lib/push";
import { type ThemePreference, readStoredThemePreference, setTheme } from "../../lib/theme";
import {
  type ReadingFont,
  type TextSize,
  readingFont,
  setReadingFont,
  setTextSize,
  textSize,
} from "../../lib/appearance";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

export default function PreferencesSection() {
  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Preferences</h2>
      </header>

      <Appearance />
      <Notifications />
    </section>
  );
}

const THEMES: readonly { value: ThemePreference; label: string; icon: typeof Sun }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

const SIZES: readonly { value: TextSize; label: string }[] = [
  { value: "small", label: "Small" },
  { value: "default", label: "Default" },
  { value: "large", label: "Large" },
];

const FONTS: readonly { value: ReadingFont; label: string }[] = [
  { value: "sans", label: "Sans" },
  { value: "serif", label: "Serif" },
  { value: "mono", label: "Mono" },
];

function Appearance() {
  const [theme, setPreference] = createSignal<ThemePreference>(readStoredThemePreference());
  const [size, setSize] = createSignal<TextSize>(textSize());
  const [font, setFont] = createSignal<ReadingFont>(readingFont());

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>Appearance</p>
      <div class={styles.insetList}>
        <Row label="Theme">
          <div class={styles.segmented} role="group" aria-label="Theme">
            <For each={THEMES}>
              {(option) => (
                <button
                  type="button"
                  class={cx(styles.segment, theme() === option.value && styles.segmentOn)}
                  aria-pressed={theme() === option.value}
                  onClick={() => {
                    setTheme(option.value);
                    setPreference(option.value);
                  }}
                >
                  <option.icon size={13} aria-hidden="true" />
                  {option.label}
                </button>
              )}
            </For>
          </div>
        </Row>
        <Row label="Interface size">
          <div class={styles.segmented} role="group" aria-label="Interface size">
            <For each={SIZES}>
              {(option) => (
                <button
                  type="button"
                  class={cx(styles.segment, size() === option.value && styles.segmentOn)}
                  aria-pressed={size() === option.value}
                  onClick={() => {
                    setTextSize(option.value);
                    setSize(option.value);
                  }}
                >
                  {option.label}
                </button>
              )}
            </For>
          </div>
        </Row>
        <Row label="Transcript font">
          <div class={styles.segmented} role="group" aria-label="Transcript font">
            <For each={FONTS}>
              {(option) => (
                <button
                  type="button"
                  class={cx(styles.segment, font() === option.value && styles.segmentOn)}
                  aria-pressed={font() === option.value}
                  onClick={() => {
                    setReadingFont(option.value);
                    setFont(option.value);
                  }}
                >
                  {option.label}
                </button>
              )}
            </For>
          </div>
        </Row>
      </div>
    </div>
  );
}

function Row(props: { label: string; children: JSX.Element }) {
  return (
    <div class={styles.insetRow}>
      <div class={styles.insetRowBar}>
        <div class={styles.identity}>
          <span class={styles.cardTitle}>{props.label}</span>
        </div>
        <div class={styles.actions}>{props.children}</div>
      </div>
    </div>
  );
}

function Notifications() {
  const [vapid] = createQuery(
    () => (isPushSupported() ? true : undefined),
    getVapidPublicKey,
  );
  const [subscribed, setSubscribed] = createSignal(false);
  const [checked, setChecked] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  onMount(() => {
    void getPushSubscription().then((subscription) => {
      setSubscribed(subscription !== null);
      setChecked(true);
    });
  });

  async function toggle(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      if (subscribed()) {
        await unsubscribeFromPush();
        setSubscribed(false);
      } else {
        await subscribeToPush();
        setSubscribed(true);
      }
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>Notifications</p>
      <div class={styles.insetList}>
        <div class={styles.insetRow}>
          <div class={styles.insetRowBar}>
            <span class={styles.mark} aria-hidden="true">
              <BellRing size={15} />
            </span>
            <div class={styles.identity}>
              <span class={styles.cardTitle}>Push notifications</span>
            </div>
            <div class={styles.actions}>
              <Show
                when={isPushSupported() && vapid.error === undefined && !vapid.loading && checked()}
                fallback={
                  <span class={styles.status}>
                    {isPushSupported() ? "Unavailable" : "Unsupported here"}
                  </span>
                }
              >
                <button
                  type="button"
                  class={subscribed() ? styles.pill : styles.pillPrimary}
                  disabled={busy()}
                  onClick={() => void toggle()}
                >
                  {busy() ? "Working…" : subscribed() ? "Turn off" : "Enable push"}
                </button>
              </Show>
            </div>
          </div>
          <ProblemNotice error={vapid.error ?? error()} />
        </div>
      </div>
    </div>
  );
}
