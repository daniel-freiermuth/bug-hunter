import { describe, expect, it } from "vitest";

import { isHttpUrl } from "./format";

/**
 * Repo URLs are operator-supplied and rendered as links. Only http(s)
 * navigates; everything else is shown as text. This covers rows stored
 * before the write path validated schemes.
 */
describe("isHttpUrl", () => {
  it.each([
    "javascript:alert(1)",
    "JavaScript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "vbscript:msgbox(1)",
  ])("refuses to link %s", (u) => {
    expect(isHttpUrl(u)).toBe(false);
  });

  it.each(["https://github.com/acme/widget.git", "http://git.internal/acme/widget.git"])(
    "links %s",
    (u) => {
      expect(isHttpUrl(u)).toBe(true);
    },
  );

  it("shows an ssh clone URL as text rather than a link", () => {
    // Not dangerous, but not navigable either.
    expect(isHttpUrl("git@github.com:acme/widget.git")).toBe(false);
  });
});
