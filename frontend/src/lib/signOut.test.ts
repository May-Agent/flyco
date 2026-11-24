import { describe, expect, it, vi } from "vitest";
import { signOut } from "./signOut";
import { clearSessionToken, getSessionToken, setSessionToken } from "./session";
import {
  consumePendingGithubLogin,
  consumeProviderOauthAttempt,
  rememberProviderOauthAttempt,
} from "../api/auth";
import { userStream } from "../api/events";

describe("signOut", () => {
  it("resets userStream cursor, unregisters push with outgoing token, clears OAuth state, and navigates to /login", async () => {
    clearSessionToken();
    setSessionToken("fs_user_a");
    localStorage.setItem("flyco.push_subscription_id", "sub-user-a");
    sessionStorage.setItem(
      "flyco.github_oauth_pending",
      JSON.stringify({ state: "gh_1", codeVerifier: "v" }),
    );
    rememberProviderOauthAttempt("azure", "az_state", "attempt-1");

    const encoder = new TextEncoder();
    const seenEventUrls: string[] = [];
    const deletedPushAuthHeaders: Array<string | null> = [];

    vi.mocked(fetch).mockImplementation((input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (url.pathname === "/v1/events") {
        seenEventUrls.push(url.pathname + url.search);
        return Promise.resolve(
          new Response(
            new ReadableStream<Uint8Array>({
              start(controller) {
                controller.enqueue(
                  encoder.encode(
                    `id: 42\ndata: ${JSON.stringify({
                      session: "s1",
                      seq: 1,
                      event: { type: "user_message", text: "hi" },
                    })}\n\n`,
                  ),
                );
              },
            }),
            { status: 200, headers: { "content-type": "text/event-stream" } },
          ),
        );
      }
      if (
        (init?.method ?? "GET").toUpperCase() === "DELETE" &&
        url.pathname === "/v1/push/subscriptions/sub-user-a"
      ) {
        const headers = new Headers(init?.headers);
        deletedPushAuthHeaders.push(headers.get("Authorization"));
        return Promise.resolve(new Response(null, { status: 204 }));
      }
      return Promise.resolve(new Response(null, { status: 204 }));
    });

    // Establish an event stream for user A that advances lastId to 42.
    const unsubA = userStream().subscribe("s1", {
      event: () => {},
      state: () => {},
      failed: () => {},
    });
    await vi.waitFor(() => expect(seenEventUrls).toEqual(["/v1/events"]));

    const navigate = vi.fn();
    signOut(navigate as never);
    unsubA();

    expect(getSessionToken()).toBeNull();
    expect(navigate).toHaveBeenCalledWith("/login", { replace: true });
    expect(deletedPushAuthHeaders).toEqual(["Bearer fs_user_a"]);
    expect(localStorage.getItem("flyco.push_subscription_id")).toBeNull();
    expect(consumePendingGithubLogin()).toBeNull();
    expect(consumeProviderOauthAttempt("azure", "az_state")).toBeNull();

    // User B signs in and opens userStream: must NOT pass ?after=42.
    setSessionToken("fs_user_b");
    const unsubB = userStream().subscribe("s2", {
      event: () => {},
      state: () => {},
      failed: () => {},
    });
    await vi.waitFor(() => expect(seenEventUrls).toHaveLength(2));
    expect(seenEventUrls[1]).toBe("/v1/events");
    unsubB();
  });
});
