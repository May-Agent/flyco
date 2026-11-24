import { Show, createSignal, onCleanup, onMount } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { consumePendingGithubLogin } from "../api/auth";
import { exchangeGithubLogin } from "../api/client";
import {
  parseGithubCallbackFragment,
  scrubLocationHash,
} from "../lib/fragmentToken";
import { consumePostLoginPath } from "../lib/postLoginPath";
import { setSessionToken } from "../lib/session";
import styles from "./AuthComplete.module.css";

/**
 * The control plane 303-redirects here with `#code=...&state=...` (or
 * `#error=...&state=...`) in the URL fragment without minting a session on
 * `GET`. This scrubs the fragment immediately, validates `state` against this
 * tab's `sessionStorage`, and posts `{ code, state, code_verifier }` to
 * `POST /v1/auth/github/exchange` to obtain the `fs_` session token.
 */
export default function AuthComplete() {
  const navigate = useNavigate();
  const [error, setError] = createSignal<string | null>(null);

  onMount(() => {
    let cancelled = false;
    onCleanup(() => {
      cancelled = true;
    });

    const rawHash = window.location.hash;
    scrubLocationHash();

    void (async () => {
      try {
        const parsed = parseGithubCallbackFragment(rawHash);
        const pending = consumePendingGithubLogin();
        if (parsed.kind === "error") {
          const detail =
            parsed.description !== null
              ? `${parsed.error}: ${parsed.description}`
              : parsed.error;
          setError(`GitHub did not complete sign-in (${detail}).`);
          return;
        }
        if (pending === null || pending.state !== parsed.state) {
          setError(
            "This sign-in callback does not match an active sign-in in this browser tab. Start sign-in again.",
          );
          return;
        }
        const session = await exchangeGithubLogin({
          code: parsed.code,
          state: parsed.state,
          code_verifier: pending.codeVerifier,
        });
        if (cancelled) {
          return;
        }
        setSessionToken(session.token);
        navigate(consumePostLoginPath(), { replace: true });
      } catch (err) {
        if (cancelled) {
          return;
        }
        setError(err instanceof Error ? err.message : String(err));
      }
    })();
  });

  return (
    <Show
      when={error()}
      fallback={
        <p class={styles.status} role="status">
          Signing you in…
        </p>
      }
    >
      <div class={styles.errorPage} role="alert">
        <h1>Sign-in failed</h1>
        <p>{error()}</p>
        <a href="/login">Back to sign-in</a>
      </div>
    </Show>
  );
}
