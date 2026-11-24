import { describe, expect, it } from "vitest";
import {
  InvalidAuthFragmentError,
  parseGithubCallbackFragment,
  parseProviderCallbackFragment,
  parseSessionTokenFragment,
  scrubLocationHash,
} from "./fragmentToken";

describe("parseSessionTokenFragment", () => {
  it("parses a valid token", () => {
    expect(parseSessionTokenFragment("#token=fs_abc123")).toBe("fs_abc123");
  });

  it("accepts the fragment without its leading #", () => {
    expect(parseSessionTokenFragment("token=fs_abc123")).toBe("fs_abc123");
  });

  it("throws when the fragment is empty", () => {
    expect(() => parseSessionTokenFragment("")).toThrow(InvalidAuthFragmentError);
    expect(() => parseSessionTokenFragment("#")).toThrow(InvalidAuthFragmentError);
  });

  it("throws when the fragment has no token field", () => {
    expect(() => parseSessionTokenFragment("#foo=bar")).toThrow(
      /missing its token field/,
    );
  });

  it("throws without embedding the raw token value when prefix is wrong", () => {
    try {
      parseSessionTokenFragment("#token=super_secret_leaked_token");
      expect.unreachable("should have thrown");
    } catch (error) {
      expect(error).toBeInstanceOf(InvalidAuthFragmentError);
      expect((error as Error).message).toMatch(/fs_ prefix/);
      expect((error as Error).message).not.toContain("super_secret_leaked_token");
    }
  });
});

describe("parseGithubCallbackFragment", () => {
  it("parses code and state", () => {
    expect(parseGithubCallbackFragment("#code=gh_code&state=gh_state")).toEqual({
      kind: "code",
      code: "gh_code",
      state: "gh_state",
    });
  });

  it("parses vendor errors", () => {
    expect(
      parseGithubCallbackFragment(
        "#error=access_denied&error_description=cancelled&state=gh_state",
      ),
    ).toEqual({
      kind: "error",
      error: "access_denied",
      description: "cancelled",
      state: "gh_state",
    });
  });

  it("rejects missing code or state without leaking raw values", () => {
    try {
      parseGithubCallbackFragment("#code=secret_code_val");
      expect.unreachable("should have thrown");
    } catch (error) {
      expect(error).toBeInstanceOf(InvalidAuthFragmentError);
      expect((error as Error).message).not.toContain("secret_code_val");
    }
  });
});

describe("parseProviderCallbackFragment", () => {
  it("parses provider, code, and state", () => {
    expect(
      parseProviderCallbackFragment("#provider=azure&code=az_code&state=az_state"),
    ).toEqual({
      kind: "code",
      provider: "azure",
      code: "az_code",
      state: "az_state",
    });
  });

  it("rejects unknown providers", () => {
    expect(() =>
      parseProviderCallbackFragment("#provider=unknown&code=c&state=s"),
    ).toThrow(InvalidAuthFragmentError);
  });
});

describe("scrubLocationHash", () => {
  it("clears window.location.hash while preserving pathname and search", () => {
    window.history.replaceState(null, "", "/auth/complete?x=1#code=secret&state=s");
    expect(window.location.hash).toBe("#code=secret&state=s");

    scrubLocationHash();

    expect(window.location.hash).toBe("");
    expect(window.location.pathname).toBe("/auth/complete");
    expect(window.location.search).toBe("?x=1");
  });
});
