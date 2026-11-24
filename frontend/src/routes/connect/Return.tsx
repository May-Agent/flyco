/**
 * `/connect/return` — where a vendor's consent screen sends the browser.
 *
 * The public callback forwards `#provider=...&code=...&state=...` (or
 * `#provider=...&error=...&state=...`) in the URL fragment without redeeming
 * the code. This page scrubs the fragment, matches `state` against the attempt
 * recorded by the flow tab in same-origin `localStorage`, and exchanges the
 * code via an authenticated `POST /v1/providers/{provider}/oauth/exchange`
 * carrying the user's Bearer token.
 */
import { useNavigate } from "@solidjs/router";
import { Match, Show, Switch, createSignal, onCleanup, onMount } from "solid-js";
import { consumeProviderOauthAttempt } from "../../api/auth";
import {
  exchangeProviderOauth,
  type ConsentCloudKind,
} from "../../api/client";
import { ApiProblem } from "../../api/problem";
import {
  parseProviderCallbackFragment,
  scrubLocationHash,
} from "../../lib/fragmentToken";
import { PROVIDER_LABEL } from "../../lib/providers";
import styles from "../../components/flow/pages/pages.module.css";
import frame from "../../components/flow/Flow.module.css";

/** Where "Try another way" goes: the compute stage, from its first page. */
const COMPUTE_STAGE = "/connect/compute";

type ReturnState =
  | { status: "exchanging"; provider: ConsentCloudKind | null }
  | { status: "succeeded"; provider: ConsentCloudKind }
  | {
      status: "failed";
      provider: ConsentCloudKind | null;
      problem: string;
      reason: string | null;
    };

/** The vendors this page can name; anything else is "the provider". */
function vendorName(provider: ConsentCloudKind | null): string {
  return provider !== null ? PROVIDER_LABEL[provider] : "the provider";
}

/** What each problem slug means to the person reading it. */
function explain(problem: string, vendor: string): string {
  switch (problem) {
    case "microsoft-rejected":
    case "google-rejected":
      return `${vendor} did not grant the sign-in. Either the consent was declined, or your organization needs an administrator to approve apps like flyco first. Cloud Shell needs no approval, and flyco can link through it instead.`;
    case "github-rejected":
      return `${vendor} did not grant the requested sign-in scopes.`;
    case "unknown-oauth-state":
    case "provider-oauth-attempt-expired":
      return "This link does not match an active sign-in in this browser, or it has already been used.";
    default:
      return "The control plane could not complete the sign-in.";
  }
}

function problemSlug(error: unknown): { slug: string; reason: string | null } {
  if (error instanceof ApiProblem) {
    const slug = error.type.split("/").pop() ?? "exchange-failed";
    return { slug, reason: error.detail };
  }
  return {
    slug: "exchange-failed",
    reason: error instanceof Error ? error.message : String(error),
  };
}

export default function ConnectReturn() {
  const navigate = useNavigate();
  const [state, setState] = createSignal<ReturnState>({
    status: "exchanging",
    provider: null,
  });

  onMount(() => {
    let cancelled = false;
    onCleanup(() => {
      cancelled = true;
    });

    const rawHash = window.location.hash;
    scrubLocationHash();

    void (async () => {
      let parsed;
      try {
        parsed = parseProviderCallbackFragment(rawHash);
      } catch {
        setState({
          status: "failed",
          provider: null,
          problem: "unknown-oauth-state",
          reason: null,
        });
        return;
      }

      setState({ status: "exchanging", provider: parsed.provider });

      const attemptId =
        parsed.state !== null
          ? consumeProviderOauthAttempt(parsed.provider, parsed.state)
          : null;

      if (parsed.kind === "error") {
        const vendorReason =
          parsed.description !== null
            ? `${parsed.error}: ${parsed.description}`
            : parsed.error;
        if (attemptId !== null && parsed.state !== null) {
          // Inform the control plane so the flow tab stops polling right away.
          try {
            await exchangeProviderOauth(parsed.provider, {
              attempt_id: attemptId,
              state: parsed.state,
              error: parsed.error,
              error_description: parsed.description,
            });
          } catch {
            // Expected: the endpoint answers 422 with the vendor refusal.
          }
        }
        if (!cancelled) {
          const slug =
            parsed.provider === "azure"
              ? "microsoft-rejected"
              : parsed.provider === "gcp"
                ? "google-rejected"
                : "github-rejected";
          setState({
            status: "failed",
            provider: parsed.provider,
            problem: slug,
            reason: vendorReason,
          });
        }
        return;
      }

      if (attemptId === null) {
        setState({
          status: "failed",
          provider: parsed.provider,
          problem: "unknown-oauth-state",
          reason: null,
        });
        return;
      }

      try {
        await exchangeProviderOauth(parsed.provider, {
          attempt_id: attemptId,
          state: parsed.state,
          code: parsed.code,
        });
        if (!cancelled) {
          setState({ status: "succeeded", provider: parsed.provider });
        }
      } catch (error) {
        if (!cancelled) {
          const { slug, reason } = problemSlug(error);
          setState({
            status: "failed",
            provider: parsed.provider,
            problem: slug,
            reason,
          });
        }
      }
    })();
  });

  const vendor = () => vendorName(state().provider);

  return (
    <div class={frame.page}>
      <section class={frame.card} aria-labelledby="return-title">
        <div class={`${frame.body} ${frame.bodyShort}`}>
          <Switch>
            <Match when={state().status === "exchanging"}>
              <h1 id="return-title" class={frame.title}>
                Completing sign-in…
              </h1>
              <p class={styles.lede} role="status">
                Confirming your {vendor()} sign-in with flyco…
              </p>
            </Match>
            <Match when={state().status === "succeeded"}>
              <h1 id="return-title" class={frame.title}>
                Signed in with {vendor()}
              </h1>
              <p class={styles.lede}>
                You can close this tab. Flyco carries on in the tab you started
                from.
              </p>
            </Match>
            <Match when={state().status === "failed"}>
              {(() => {
                const current = () =>
                  state() as Extract<ReturnState, { status: "failed" }>;
                return (
                  <>
                    <h1 id="return-title" class={frame.title}>
                      {vendor()} did not complete the sign-in
                    </h1>
                    <p class={styles.lede}>
                      {explain(current().problem, vendor())}
                    </p>
                    <Show when={current().reason}>
                      {(reason) => (
                        <p class={styles.hint}>
                          {vendor()} said: <code>{reason()}</code>
                        </p>
                      )}
                    </Show>
                  </>
                );
              })()}
            </Match>
          </Switch>
        </div>
        <Show when={state().status === "failed"}>
          <footer class={frame.footer}>
            <button
              type="button"
              class={frame.primary}
              onClick={() => navigate(COMPUTE_STAGE)}
            >
              Try another way
            </button>
          </footer>
        </Show>
      </section>
    </div>
  );
}
