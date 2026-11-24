import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  clearSessionToken,
  getSessionToken,
  isSignedIn,
  onSessionChanged,
  resetSessionTokenCacheForTests,
  setSessionToken,
} from "./session";

describe("session token storage", () => {
  beforeEach(() => {
    resetSessionTokenCacheForTests();
  });

  it("round-trips a valid token", () => {
    setSessionToken("fs_abc123");
    expect(getSessionToken()).toBe("fs_abc123");
    expect(isSignedIn()).toBe(true);
  });

  it("rejects a token without the fs_ prefix", () => {
    expect(() => setSessionToken("bad_token")).toThrow(/unexpected shape/);
  });

  it("reports null and signed-out when nothing is stored", () => {
    expect(getSessionToken()).toBeNull();
    expect(isSignedIn()).toBe(false);
  });

  it("survives a simulated reload by falling back to localStorage", () => {
    setSessionToken("fs_abc123");
    resetSessionTokenCacheForTests();
    expect(getSessionToken()).toBe("fs_abc123");
  });

  it("clears both the memory cache and localStorage", () => {
    setSessionToken("fs_abc123");
    clearSessionToken();
    resetSessionTokenCacheForTests();
    expect(getSessionToken()).toBeNull();
  });

  it("synchronizes memoryToken across tabs via storage events", () => {
    const listener = vi.fn();
    const stop = onSessionChanged(listener);
    try {
      setSessionToken("fs_user_a");
      listener.mockClear();

      // Another tab signs in as user B.
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "flyco.session_token",
          oldValue: "fs_user_a",
          newValue: "fs_user_b",
        }),
      );
      expect(getSessionToken()).toBe("fs_user_b");
      expect(listener).toHaveBeenCalledTimes(1);

      // Another tab signs out.
      localStorage.removeItem("flyco.session_token");
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "flyco.session_token",
          oldValue: "fs_user_b",
          newValue: null,
        }),
      );
      expect(getSessionToken()).toBeNull();
      expect(listener).toHaveBeenCalledTimes(2);
    } finally {
      stop();
    }
  });
});

describe("credential hygiene", () => {
  it("keeps the token out of the error it throws", () => {
    resetSessionTokenCacheForTests();
    expect(() => setSessionToken("leaky_secret_value")).toThrow(
      /unexpected shape/,
    );
    try {
      setSessionToken("leaky_secret_value");
    } catch (error) {
      expect((error as Error).message).not.toContain("leaky_secret_value");
    }
  });

  it("treats a corrupted stored token as signed out and clears it", () => {
    resetSessionTokenCacheForTests();
    const store = new Map<string, string>([[
      "flyco.session_token",
      "not-a-flyco-token",
    ]]);
    const storage = {
      getItem: (key: string) => store.get(key) ?? null,
      removeItem: (key: string) => void store.delete(key),
    };

    expect(getSessionToken(storage)).toBeNull();
    expect(store.has("flyco.session_token")).toBe(false);
  });
});
