/**
 * Cookie-less OAuth flow helpers for GitHub sign-in and cloud provider linking.
 *
 * GitHub sign-in binds the initiating browser tab via `sessionStorage` + S256
 * PKCE: `beginGithubLogin` generates a verifier, sends its challenge to
 * `POST /v1/auth/github/start`, stores `{ state, codeVerifier }` in
 * `sessionStorage`, and navigates to GitHub. When `/v1/auth/github/callback`
 * redirects to `/auth/complete#code=...&state=...`, `AuthComplete` matches
 * `state` locally and calls `POST /v1/auth/github/exchange` with
 * `code_verifier`.
 *
 * Cloud provider linking opens the vendor in a new `noopener` tab (which gets
 * a fresh `sessionStorage` but shares same-origin `localStorage`), so pending
 * `{ attemptId, provider, state }` entries live in `localStorage` with a
 * 30-minute TTL and are cleared on sign-out.
 */
import { ApiProblem } from "./problem";
import {
  getPublicConfig,
  startGithubLogin,
  type ConsentCloudKind,
} from "./client";
import { acquireTurnstileToken } from "../lib/turnstile";

const GITHUB_LOGIN_STORAGE_KEY = "flyco.github_oauth_pending";
const PROVIDER_ATTEMPTS_STORAGE_KEY = "flyco.provider_oauth_attempts";
const PROVIDER_ATTEMPT_TTL_MS = 30 * 60 * 1000;

export interface PendingGithubLogin {
  state: string;
  codeVerifier: string;
}

interface StoredProviderAttempt {
  attemptId: string;
  provider: ConsentCloudKind;
  createdAt: number;
}

function base64UrlEncode(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 1) {
    binary += String.fromCharCode(bytes[i]!);
  }
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/u, "");
}

/** Generates a 32-byte URL-safe base64 PKCE verifier (43 chars). */
export function generateCodeVerifier(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return base64UrlEncode(bytes);
}

/** Computes the RFC 7636 S256 PKCE challenge for `verifier`. */
export async function computeCodeChallenge(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(verifier),
  );
  return base64UrlEncode(new Uint8Array(digest));
}

/**
 * Whether `error` is GitHub refusing the token flyco holds for the user.
 *
 * The session is fine; the credential died. The way out is a new GitHub
 * authorization from wherever the user is, never a sign-out.
 */
export function githubTokenRevoked(error: unknown): boolean {
  return error instanceof ApiProblem && error.type.endsWith("/github-token-revoked");
}

export async function beginGithubLogin(turnstileToken?: string): Promise<void> {
  let token = turnstileToken;
  if (token === undefined) {
    // Callers without a widget of their own — the "Reconnect GitHub"
    // affordances — prove the human through a hidden one on demand.
    const { turnstile_sitekey: sitekey } = await getPublicConfig();
    token = await acquireTurnstileToken(sitekey);
  }
  const codeVerifier = generateCodeVerifier();
  const codeChallenge = await computeCodeChallenge(codeVerifier);
  const { authorize_url: authorizeUrl } = await startGithubLogin(
    token,
    codeChallenge,
  );
  const state = new URL(authorizeUrl).searchParams.get("state");
  if (state === null || state === "") {
    throw new Error("GitHub sign-in URL is missing its state parameter");
  }
  sessionStorage.setItem(
    GITHUB_LOGIN_STORAGE_KEY,
    JSON.stringify({ state, codeVerifier } satisfies PendingGithubLogin),
  );
  window.location.assign(authorizeUrl);
}

/** Reads and removes the one-shot GitHub sign-in state for this tab. */
export function consumePendingGithubLogin(
  storage: Pick<Storage, "getItem" | "removeItem"> = sessionStorage,
): PendingGithubLogin | null {
  const raw = storage.getItem(GITHUB_LOGIN_STORAGE_KEY);
  storage.removeItem(GITHUB_LOGIN_STORAGE_KEY);
  if (raw === null) {
    return null;
  }
  try {
    const parsed = JSON.parse(raw) as Partial<PendingGithubLogin>;
    if (
      typeof parsed.state === "string" &&
      parsed.state.length > 0 &&
      typeof parsed.codeVerifier === "string" &&
      parsed.codeVerifier.length > 0
    ) {
      return { state: parsed.state, codeVerifier: parsed.codeVerifier };
    }
  } catch {
    // Corrupt storage entry; treat as absent.
  }
  return null;
}

function readProviderAttempts(
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem">,
): Record<string, StoredProviderAttempt> {
  const raw = storage.getItem(PROVIDER_ATTEMPTS_STORAGE_KEY);
  if (raw === null) {
    return {};
  }
  try {
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
      storage.removeItem(PROVIDER_ATTEMPTS_STORAGE_KEY);
      return {};
    }
    const now = Date.now();
    const fresh: Record<string, StoredProviderAttempt> = {};
    for (const [state, entry] of Object.entries(parsed)) {
      const candidate = entry as Partial<StoredProviderAttempt> | null;
      if (
        candidate !== null &&
        typeof candidate === "object" &&
        typeof candidate.attemptId === "string" &&
        candidate.attemptId.length > 0 &&
        (candidate.provider === "azure" ||
          candidate.provider === "gcp" ||
          candidate.provider === "codespaces") &&
        typeof candidate.createdAt === "number" &&
        now - candidate.createdAt < PROVIDER_ATTEMPT_TTL_MS
      ) {
        fresh[state] = {
          attemptId: candidate.attemptId,
          provider: candidate.provider,
          createdAt: candidate.createdAt,
        };
      }
    }
    return fresh;
  } catch {
    storage.removeItem(PROVIDER_ATTEMPTS_STORAGE_KEY);
    return {};
  }
}

function writeProviderAttempts(
  storage: Pick<Storage, "setItem" | "removeItem">,
  attempts: Record<string, StoredProviderAttempt>,
): void {
  if (Object.keys(attempts).length === 0) {
    storage.removeItem(PROVIDER_ATTEMPTS_STORAGE_KEY);
  } else {
    storage.setItem(PROVIDER_ATTEMPTS_STORAGE_KEY, JSON.stringify(attempts));
  }
}

/** Records a newly started cloud provider OAuth attempt for `/connect/return`. */
export function rememberProviderOauthAttempt(
  provider: ConsentCloudKind,
  state: string,
  attemptId: string,
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = localStorage,
): void {
  const attempts = readProviderAttempts(storage);
  attempts[state] = { attemptId, provider, createdAt: Date.now() };
  writeProviderAttempts(storage, attempts);
}

/** Finds and consumes the local `attemptId` bound to `(provider, state)`. */
export function consumeProviderOauthAttempt(
  provider: ConsentCloudKind,
  state: string,
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = localStorage,
): string | null {
  const attempts = readProviderAttempts(storage);
  const found = attempts[state];
  if (found === undefined || found.provider !== provider) {
    return null;
  }
  delete attempts[state];
  writeProviderAttempts(storage, attempts);
  return found.attemptId;
}

/** Removes a pending provider attempt when the user cancels or restarts. */
export function forgetProviderOauthAttempt(
  attemptId: string,
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = localStorage,
): void {
  const attempts = readProviderAttempts(storage);
  let changed = false;
  for (const [state, entry] of Object.entries(attempts)) {
    if (entry.attemptId === attemptId) {
      delete attempts[state];
      changed = true;
    }
  }
  if (changed) {
    writeProviderAttempts(storage, attempts);
  }
}

/** Clears any in-flight OAuth transaction state on sign-out. */
export function clearPendingOauthStorage(): void {
  try {
    sessionStorage.removeItem(GITHUB_LOGIN_STORAGE_KEY);
  } catch {
    // Ignore storage unavailability during sign-out.
  }
  try {
    localStorage.removeItem(PROVIDER_ATTEMPTS_STORAGE_KEY);
  } catch {
    // Ignore storage unavailability during sign-out.
  }
}
