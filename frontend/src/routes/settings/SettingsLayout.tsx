/**
 * Two-column Settings layout for direct `/settings/*` routes.
 *
 * Keeps the main session rail intact on the left of the app and presents the
 * unified two-column settings window in the workspace area.
 */
import { type JSX } from "solid-js";
import SettingsNav from "../../components/SettingsNav";
import styles from "./Settings.module.css";

export default function SettingsLayout(props: { children?: JSX.Element }) {
  return (
    <div class={styles.page}>
      <h1 class="visually-hidden">Settings</h1>
      <div class={styles.window}>
        <aside class={styles.sidebar}>
          <p class={styles.sidebarTitle}>Settings</p>
          <SettingsNav />
        </aside>
        <div class={styles.content}>{props.children}</div>
      </div>
    </div>
  );
}
