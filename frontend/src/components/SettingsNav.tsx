/**
 * Settings section definitions shared between the two-column Settings modal
 * and direct `/settings/*` routes.
 */
import { For } from "solid-js";
import { A } from "@solidjs/router";
import {
  Blocks,
  Bot,
  CircleUser,
  FileText,
  KeyRound,
  Server,
  SlidersHorizontal,
} from "lucide-solid";
import { cx } from "../lib/cx";
import styles from "./SettingsNav.module.css";

export type SettingsSectionId =
  | "agents"
  | "compute"
  | "tools"
  | "instructions"
  | "api-keys"
  | "preferences"
  | "account";

export interface SettingsSection {
  id: SettingsSectionId;
  href: string;
  label: string;
  icon: typeof Bot;
}

export const SETTINGS_SECTIONS: readonly SettingsSection[] = [
  { id: "agents", href: "/settings/agents", label: "Agents", icon: Bot },
  { id: "compute", href: "/settings/compute", label: "Compute", icon: Server },
  { id: "tools", href: "/settings/tools", label: "Tools", icon: Blocks },
  { id: "instructions", href: "/settings/instructions", label: "Instructions", icon: FileText },
  { id: "api-keys", href: "/settings/api-keys", label: "API keys", icon: KeyRound },
  { id: "preferences", href: "/settings/preferences", label: "Preferences", icon: SlidersHorizontal },
  { id: "account", href: "/settings/account", label: "Account", icon: CircleUser },
];

export default function SettingsNav(props: {
  active?: SettingsSectionId | undefined;
  onSelect?: ((id: SettingsSectionId) => void) | undefined;
  onNavigate?: (() => void) | undefined;
}) {
  return (
    <nav class={styles.nav} aria-label="Settings sections">
      <For each={SETTINGS_SECTIONS}>
        {(section) =>
          props.onSelect !== undefined ? (
            <button
              type="button"
              class={cx(styles.link, props.active === section.id && styles.linkActive)}
              aria-current={props.active === section.id ? "page" : undefined}
              onClick={() => {
                props.onSelect?.(section.id);
                props.onNavigate?.();
              }}
            >
              <section.icon size={15} aria-hidden="true" />
              {section.label}
            </button>
          ) : (
            <A
              href={section.href}
              class={styles.link}
              activeClass={styles.linkActive}
              onClick={() => props.onNavigate?.()}
            >
              <section.icon size={15} aria-hidden="true" />
              {section.label}
            </A>
          )
        }
      </For>
    </nav>
  );
}
