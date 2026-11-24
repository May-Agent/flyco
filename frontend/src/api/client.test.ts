import { describe, expect, it, vi } from "vitest";
import {
  apiPath,
  denyCliSession,
  encodePathSegment,
  getMe,
  getSession,
} from "./client";
import { clearSessionToken, getSessionToken, setSessionToken } from "../lib/session";

describe("API path encoding", () => {
  it("encodes path separators and fragments so Deny can never become Approve", async () => {
    setSessionToken("fs_client_test");
    const payload = "11111111-2222-4333-8444-555555555555/approve#";
    vi.mocked(fetch).mockResolvedValueOnce(new Response(null, { status: 204 }));

    await denyCliSession(payload);

    const [input, init] = vi.mocked(fetch).mock.calls[0]!;
    const url = new URL(String(input));
    expect(init?.method).toBe("POST");
    expect(url.pathname).toBe(
      "/v1/cli-sessions/11111111-2222-4333-8444-555555555555%2Fapprove%23/deny",
    );
    expect(url.pathname.endsWith("/approve")).toBe(false);
    expect(url.hash).toBe("");
  });

  it("rejects dot-segment traversal and empty segments", () => {
    expect(() => encodePathSegment("")).toThrow(/Invalid API path segment/);
    expect(() => encodePathSegment(".")).toThrow(/Invalid API path segment/);
    expect(() => encodePathSegment("..")).toThrow(/Invalid API path segment/);
    expect(() => apiPath`/v1/cli-sessions/${".."}/deny`).toThrow(
      /Invalid API path segment/,
    );
    expect(() => getSession("..")).toThrow(/Invalid API path segment/);
  });
});

describe("stale 401 token clearing", () => {
  it("does not clear a newer session token when a stale request returns 401", async () => {
    clearSessionToken();
    setSessionToken("fs_old_session");

    let resolveFetch!: (response: Response) => void;
    vi.mocked(fetch).mockReturnValueOnce(
      new Promise<Response>((resolve) => {
        resolveFetch = resolve;
      }),
    );

    const inflight = getMe();
    // User switches session before the old request completes.
    setSessionToken("fs_new_session");

    resolveFetch(
      new Response(
        JSON.stringify({
          type: "https://flyco.dev/problems/invalid-credential",
          title: "Unauthorized",
          status: 401,
          detail: "expired",
        }),
        {
          status: 401,
          headers: { "content-type": "application/problem+json" },
        },
      ),
    );

    await expect(inflight).rejects.toThrow();
    expect(getSessionToken()).toBe("fs_new_session");
  });

  it("clears the current session token when a matching request returns 401", async () => {
    clearSessionToken();
    setSessionToken("fs_current_session");
    vi.mocked(fetch).mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          type: "https://flyco.dev/problems/invalid-credential",
          title: "Unauthorized",
          status: 401,
          detail: "expired",
        }),
        {
          status: 401,
          headers: { "content-type": "application/problem+json" },
        },
      ),
    );

    await expect(getMe()).rejects.toThrow();
    expect(getSessionToken()).toBeNull();
  });
});
