/**
 * Centred two-column Settings modal (`<dialog>`), opened over the current
 * page without leaving an active session.
 */
import { Match, Switch, createSignal, onCleanup, onMount } from "solid-js";
import { X } from "lucide-solid";
import SettingsNav, { type SettingsSectionId } from "./SettingsNav";
import AgentsSection from "../routes/settings/AgentsSection";
import ComputeSection from "../routes/settings/ComputeSection";
import ToolsSection from "../routes/settings/ToolsSection";
import InstructionsSection from "../routes/settings/InstructionsSection";
import ApiKeysSection from "../routes/settings/ApiKeysSection";
import PreferencesSection from "../routes/settings/PreferencesSection";
import { AccountSectionView } from "../routes/settings/AccountSection";
import styles from "../routes/settings/Settings.module.css";

export interface SettingsModalProps {
  initialSection?: SettingsSectionId | undefined;
  onClose: () => void;
}

export default function SettingsModal(props: SettingsModalProps) {
  let dialog!: HTMLDialogElement;
  const [section, setSection] = createSignal<SettingsSectionId>(
    props.initialSection ?? "agents",
  );

  onMount(() => {
    dialog.showModal();
  });

  onCleanup(() => {
    if (dialog.open) {
      dialog.close();
    }
  });

  return (
    <dialog
      ref={dialog}
      class={styles.modalDialog}
      aria-label="Settings"
      onCancel={(event) => {
        event.preventDefault();
        props.onClose();
      }}
      onClick={(event) => {
        if (event.target === dialog) {
          props.onClose();
        }
      }}
    >
      <div class={styles.modalShell}>
        <aside class={styles.sidebar}>
          <div class={styles.sidebarHeader}>
            <button
              type="button"
              class={styles.closeButton}
              aria-label="Close settings"
              onClick={props.onClose}
            >
              <X size={15} aria-hidden="true" />
            </button>
            <span class={styles.sidebarTitle}>Settings</span>
          </div>
          <SettingsNav active={section()} onSelect={setSection} />
        </aside>
        <div class={styles.modalContent}>
          <Switch>
            <Match when={section() === "agents"}>
              <AgentsSection />
            </Match>
            <Match when={section() === "compute"}>
              <ComputeSection />
            </Match>
            <Match when={section() === "tools"}>
              <ToolsSection />
            </Match>
            <Match when={section() === "instructions"}>
              <InstructionsSection />
            </Match>
            <Match when={section() === "api-keys"}>
              <ApiKeysSection />
            </Match>
            <Match when={section() === "preferences"}>
              <PreferencesSection />
            </Match>
            <Match when={section() === "account"}>
              <AccountSectionView onSignedOut={props.onClose} />
            </Match>
          </Switch>
        </div>
      </div>
    </dialog>
  );
}
