/**
 * Signing out, in one place.
 *
 * Two screens offer it — the account menu in the top bar and the Account
 * section of settings — and they must do the same cleanup: best-effort
 * unregister web push while the outgoing session token is still available,
 * drop pending OAuth transaction state, reset the shared event stream and its
 * cursor, clear the session token, and replace the current history entry with
 * the sign-in page.
 */
import type { useNavigate } from "@solidjs/router";
import { clearPendingOauthStorage } from "../api/auth";
import { resetUserStream } from "../api/events";
import { unregisterPushOnSignOut } from "./push";
import { clearSessionToken } from "./session";

export function signOut(navigate: ReturnType<typeof useNavigate>): void {
  unregisterPushOnSignOut();
  clearPendingOauthStorage();
  resetUserStream();
  clearSessionToken();
  navigate("/login", { replace: true });
}
