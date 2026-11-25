/**
 * Shared composer container used by both the home composer (with a recessed
 * lower context tray) and the in-session composer (single card).
 */
import { type JSX, Show } from "solid-js";
import { cx } from "../lib/cx";
import styles from "./Composer.module.css";

export interface ComposerShellProps {
  /** The prompt text. Controlled by the caller. */
  value: string;
  onInput: (value: string) => void;
  /** Sends. Called by the submit key and by whatever `action` renders. */
  onSubmit: () => void;
  placeholder: string;
  /** What the field is, for assistive technology. */
  label: string;
  disabled?: boolean | undefined;
  /** Extra key handling, e.g. a command palette. Returning `true` consumes the event. */
  onKeyDown?: ((event: KeyboardEvent) => boolean) | undefined;
  /** Controls at the bottom-left inside the primary card. */
  controls?: JSX.Element | undefined;
  /** Controls at the bottom-right inside the primary card, beside `action`. */
  trailing?: JSX.Element | undefined;
  /** The right-hand action: a send button, or a stop button. */
  action: JSX.Element;
  /** Optional recessed context tray nested directly beneath the primary card. */
  tray?: JSX.Element | undefined;
  /** A panel floating over the field, e.g. the command palette. */
  overlay?: JSX.Element | undefined;
  /** Anything under the container: a hint, an error. */
  children?: JSX.Element | undefined;
  /** Hands the textarea back, so a caller can focus it. */
  ref?: ((element: HTMLTextAreaElement) => void) | undefined;
}

export default function ComposerShell(props: ComposerShellProps) {
  function onKeyDown(event: KeyboardEvent): void {
    if (props.onKeyDown?.(event) === true) {
      return;
    }
    if (event.key !== "Enter" || event.shiftKey) {
      return;
    }
    event.preventDefault();
    props.onSubmit();
  }

  return (
    <div class={styles.stack}>
      <div class={cx(styles.composer, styles.composerRelative)}>
        {props.overlay}
        <textarea
          class={styles.prompt}
          placeholder={props.placeholder}
          aria-label={props.label}
          value={props.value}
          rows="3"
          disabled={props.disabled ?? false}
          ref={props.ref}
          onInput={(event) => props.onInput(event.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <div class={styles.controls}>
          <Show when={props.controls}>
            <div class={styles.chips}>{props.controls}</div>
          </Show>
          <div class={styles.trailingGroup}>
            {props.trailing}
            {props.action}
          </div>
        </div>
      </div>
      <Show when={props.tray}>
        <div class={styles.tray}>{props.tray}</div>
      </Show>
      <Show when={props.children}>{props.children}</Show>
    </div>
  );
}
