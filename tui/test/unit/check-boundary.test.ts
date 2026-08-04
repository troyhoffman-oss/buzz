/**
 * Tests for the `tui-check-boundary` gate — DESIGN.md §6.4.
 *
 * A boundary gate can fail in two directions, and they are not equally bad. A
 * false *positive* is loud and gets fixed in minutes. A false *negative* is
 * silent, and it means protocol knowledge has already leaked into the half the
 * design calls disposable. Both directions are covered below, with the
 * false-negative cases written as regressions for bugs this gate actually had.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";

const SCRIPT = new URL("../../scripts/check-boundary.sh", import.meta.url)
  .pathname;
const CWD = new URL("../../", import.meta.url).pathname;
const PROBE_DIR = `${CWD}src/__boundary_probe__`;

/** Run the gate with one probe file present. Returns its exit code. */
function runWithProbe(name: string, contents: string): number {
  mkdirSync(PROBE_DIR, { recursive: true });
  writeFileSync(`${PROBE_DIR}/${name}`, contents);
  const result = Bun.spawnSync(["bash", SCRIPT], { cwd: CWD });
  return result.exitCode;
}

afterEach(() => {
  rmSync(PROBE_DIR, { recursive: true, force: true });
});

describe("clean source passes", () => {
  test("the real src/ tree holds no protocol knowledge", () => {
    const result = Bun.spawnSync(["bash", SCRIPT], { cwd: CWD });
    expect(result.exitCode).toBe(0);
  });

  test("ordinary front-end code does not trip the gate", () => {
    // Topic names, small numbers, and a string that merely contains comment
    // characters must all be fine, or the gate is unusable.
    expect(
      runWithProbe(
        "ok.ts",
        [
          'export const topic = "message.new";',
          "export const timeoutMs = 150;",
          'export const s = "a /* not a comment */ b";',
        ].join("\n"),
      ),
    ).toBe(0);
  });
});

describe("violations are caught", () => {
  test("a bare event kind", () => {
    expect(runWithProbe("kind.ts", "export const k = 40002;\n")).toBe(1);
  });

  test("a kind on a constant not named 'kind'", () => {
    // Regression: the gate once matched only `kind`-adjacent numbers, so
    // `OBSERVER_FRAME = 24200` sailed through. §6.4 says "any bare event-kind
    // integer in the 4-digit-and-up range", not "any number next to the word
    // kind".
    expect(
      runWithProbe("named.ts", "export const OBSERVER_FRAME = 24200;\n"),
    ).toBe(1);
  });

  test("a kind on a line after a URL literal", () => {
    // Regression: the comment stripper used `sub(/\/\/.*$/, "")`, which
    // truncated at the `//` inside `https://` — so anything after a URL, on
    // that line or a later one, silently vanished from the scan.
    expect(
      runWithProbe(
        "url.ts",
        [
          'export const url = "https://example.com/x";',
          "export const k = 40002;",
        ].join("\n"),
      ),
    ).toBe(1);
  });

  test("a raw nsec reference", () => {
    expect(
      runWithProbe("nsec.ts", 'export const body = { nsec: "x" };\n'),
    ).toBe(1);
  });

  test("a cursor decode", () => {
    expect(
      runWithProbe("cursor.ts", "export const d = (c: string) => atob(c);\n"),
    ).toBe(1);
  });

  test("a literal hex colour", () => {
    expect(runWithProbe("colour.ts", 'export const c = "#ff00aa";\n')).toBe(1);
  });
});

describe("comments do not trip the gate", () => {
  test("a doc comment may explain the rule it documents", () => {
    // Otherwise the next author's cheapest fix is to delete the explanation,
    // which is the opposite of what the gate is for.
    expect(
      runWithProbe(
        "documented.ts",
        [
          "/**",
          " * The TUI never sees a kind: 40002, and POST /session/identity has",
          " * no {nsec} field. Colours are tokens, never #ff00aa.",
          " */",
          "export const ok = true;",
        ].join("\n"),
      ),
    ).toBe(0);
  });

  test("a line comment is stripped too", () => {
    expect(
      runWithProbe("line.ts", "export const ok = true; // kind 40002\n"),
    ).toBe(0);
  });
});
