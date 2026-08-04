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

  test("a same-prefixed unrelated package is not a protocol import", () => {
    // The import rule must anchor on the package root plus a subpath or quote,
    // or every future package starting with "nostr" becomes unusable and the
    // gate becomes something authors route around.
    expect(
      runWithProbe(
        "prefix.ts",
        'import { Widget } from "nostrich-ui";\nexport const w = Widget;\n',
      ),
    ).toBe(0);
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

  test("the kind-scan exemption is exact-path, not prefix or glob", () => {
    // `src/time/units.ts` is exempted from rule 2 so `1000` has one auditable
    // home (see KIND_SCAN_EXEMPT in the script). The exemption must not extend
    // to a sibling: an exemption that matched by directory or prefix would turn
    // "one file of time constants" into a place kinds could be parked, which is
    // the false negative the blanket digit rule exists to prevent.
    expect(runWithProbe("units.ts", "export const k = 40002;\n")).toBe(1);
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

  test("an actual bech32 nsec literal", () => {
    // Regression, and the worst false negative this gate has had: the rule was
    // `\bnsec\b`, whose trailing boundary cannot match `nsec1…` — `1` is a word
    // character. So the gate caught the *identifier* `nsec` while a real
    // secret-key literal pasted into src/ passed clean, which inverts the
    // rule's entire purpose.
    expect(
      runWithProbe(
        "literal.ts",
        'export const k = "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqq";\n',
      ),
    ).toBe(1);
  });

  test("an ncryptsec literal", () => {
    // §2.5 puts `ncryptsec1` in the daemon's redactor superset; the encrypted
    // form has no more business in the front end than the raw one.
    expect(
      runWithProbe("encrypted.ts", 'export const k = "ncryptsec1abc";\n'),
    ).toBe(1);
  });

  test("a protocol package imported without being declared", () => {
    // Regression: rule 1 scanned only package.json, so a transitive,
    // workspace-linked, or hoisted package was importable without ever being
    // declared. §6.4 is about what src/ *knows*, and an import is exactly that.
    expect(
      runWithProbe(
        "import.ts",
        'import { getPublicKey } from "nostr-tools";\nexport const p = getPublicKey;\n',
      ),
    ).toBe(1);
  });

  test("a protocol package imported by subpath", () => {
    expect(
      runWithProbe(
        "subpath.ts",
        'import { schnorr } from "@noble/curves/secp256k1";\nexport const s = schnorr;\n',
      ),
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
