/**
 * Parses OAuth callback parameters out of redirect URL fragments and scrubs
 * sensitive parameters from `window.location.hash`.
 *
 * Parameters travel in the URL fragment so they never reach server access
 * logs or `Referer` headers. Error messages deliberately omit raw credentials,
 * codes, and state values so nothing sensitive is rendered to the DOM or logs.
 */
import type { ConsentCloudKind } from "../api/client";

export class InvalidAuthFragmentError extends Error {}

export type GithubCallbackFragment =
  | { kind: "code"; code: string; state: string }
  | {
      kind: "error";
      error: string;
      description: string | null;
      state: string | null;
    };

export type ProviderCallbackFragment =
  | { kind: "code"; provider: ConsentCloudKind; code: string; state: string }
  | {
      kind: "error";
      provider: ConsentCloudKind;
      error: string;
      description: string | null;
      state: string | null;
    };

function stripLeadingHash(fragment: string): string {
  return fragment.startsWith("#") ? fragment.slice(1) : fragment;
}

function isConsentCloud(value: string | null): value is ConsentCloudKind {
  return value === "azure" || value === "gcp" || value === "codespaces";
}

/**
 * Removes any URL fragment from the current history entry without reloading
 * the page, keeping OAuth codes/tokens out of browser history and the address
 * bar.
 */
export function scrubLocationHash(): void {
  if (typeof window !== "undefined" && window.location.hash !== "") {
    window.history.replaceState(
      null,
      "",
      `${window.location.pathname}${window.location.search}`,
    );
  }
}

/**
 * Parses `/auth/complete#code=...&state=...` (or `#error=...&state=...`).
 *
 * @throws {InvalidAuthFragmentError} if the fragment is empty or missing
 *   required callback fields.
 */
export function parseGithubCallbackFragment(
  fragment: string,
): GithubCallbackFragment {
  const withoutHash = stripLeadingHash(fragment);
  if (withoutHash.length === 0) {
    throw new InvalidAuthFragmentError("Auth redirect had no callback parameters");
  }

  const params = new URLSearchParams(withoutHash);
  const error = params.get("error");
  const state = params.get("state");
  if (error !== null && error.length > 0) {
    const description = params.get("error_description");
    return {
      kind: "error",
      error,
      description: description !== null && description.length > 0 ? description : null,
      state: state !== null && state.length > 0 ? state : null,
    };
  }

  const code = params.get("code");
  if (code === null || code.length === 0) {
    throw new InvalidAuthFragmentError(
      "Auth redirect fragment is missing its authorization code",
    );
  }
  if (state === null || state.length === 0) {
    throw new InvalidAuthFragmentError(
      "Auth redirect fragment is missing its state parameter",
    );
  }
  return { kind: "code", code, state };
}

/**
 * Parses `/connect/return#provider=...&code=...&state=...` (or `#provider=...&error=...&state=...`).
 *
 * @throws {InvalidAuthFragmentError} if the fragment is empty, names an
 *   unknown provider, or omits required callback fields.
 */
export function parseProviderCallbackFragment(
  fragment: string,
): ProviderCallbackFragment {
  const withoutHash = stripLeadingHash(fragment);
  if (withoutHash.length === 0) {
    throw new InvalidAuthFragmentError(
      "Provider redirect had no callback parameters",
    );
  }

  const params = new URLSearchParams(withoutHash);
  const provider = params.get("provider");
  if (!isConsentCloud(provider)) {
    throw new InvalidAuthFragmentError(
      "Provider redirect named an unrecognized provider",
    );
  }

  const state = params.get("state");
  const error = params.get("error");
  if (error !== null && error.length > 0) {
    const description = params.get("error_description");
    return {
      kind: "error",
      provider,
      error,
      description: description !== null && description.length > 0 ? description : null,
      state: state !== null && state.length > 0 ? state : null,
    };
  }

  const code = params.get("code");
  if (code === null || code.length === 0) {
    throw new InvalidAuthFragmentError(
      "Provider redirect fragment is missing its authorization code",
    );
  }
  if (state === null || state.length === 0) {
    throw new InvalidAuthFragmentError(
      "Provider redirect fragment is missing its state parameter",
    );
  }
  return { kind: "code", provider, code, state };
}

/**
 * @param fragment `location.hash`, with or without its leading `#`.
 * @throws {InvalidAuthFragmentError} if the fragment is empty, has no
 *   `token` field, or the token doesn't have the `fs_` session-token shape.
 */
export function parseSessionTokenFragment(fragment: string): string {
  const withoutHash = stripLeadingHash(fragment);
  if (withoutHash.length === 0) {
    throw new InvalidAuthFragmentError(
      "Auth redirect had no fragment; expected #token=fs_...",
    );
  }

  const params = new URLSearchParams(withoutHash);
  const token = params.get("token");
  if (token === null || token.length === 0) {
    throw new InvalidAuthFragmentError(
      "Auth redirect fragment is missing its token field",
    );
  }
  if (!token.startsWith("fs_")) {
    throw new InvalidAuthFragmentError(
      "Auth redirect token does not have the expected fs_ prefix",
    );
  }

  return token;
}
