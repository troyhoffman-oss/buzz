/**
 * T0 — the first-run wizard (DESIGN.md §4's owner directive, §2.5).
 *
 * The flow is a pure reducer, so the whole journey is a table of keypresses,
 * exactly as §4's walkthroughs are for the main navigation. What a PTY then
 * adds (`test/tmux/onboarding.test.ts`) is proof that the keys *arrive* — which
 * is the only thing a PTY is actually needed for.
 *
 * Two of these cases are security properties rather than UX ones, and they are
 * the reason this file exists at all: **nothing secret is stored in the visible
 * field**, and **nothing secret is rendered back**.
 */

import { describe, expect, test } from "bun:test";
import {
  MIN_PASSPHRASE_LEN,
  type OnboardingKey,
  type OnboardingState,
  applyOnboardingKey,
  initialOnboarding,
  isSecretStep,
  onboardingFailed,
  onboardingSucceeded,
  previousStep,
  takeOnboardingEffect,
  validatePassphrase,
  validateRelay,
} from "../../src/onboarding/flow";
import { promptFor, renderOnboarding } from "../../src/onboarding/screen";

/** Type a string one printable at a time, as a terminal delivers it. */
function type(state: OnboardingState, text: string): OnboardingState {
  let next = state;
  for (const char of text) next = applyOnboardingKey(next, { name: "", char });
  return next;
}

const ENTER: OnboardingKey = { name: "return" };
const ESC: OnboardingKey = { name: "escape" };
const DOWN: OnboardingKey = { name: "down" };

const PASSPHRASE = "correct horse battery";

/** Walk the create path to the point of provisioning. */
function walkCreate(): OnboardingState {
  let state = initialOnboarding();
  state = applyOnboardingKey(state, ENTER); // welcome → relay
  state = type(state, "wss://relay.example");
  state = applyOnboardingKey(state, ENTER); // → identityChoice
  state = applyOnboardingKey(state, ENTER); // create → passphrase
  state = type(state, PASSPHRASE);
  state = applyOnboardingKey(state, ENTER); // → confirm
  state = type(state, PASSPHRASE);
  state = applyOnboardingKey(state, ENTER); // → community
  state = type(state, "buzz-dev");
  return applyOnboardingKey(state, ENTER); // → provisioning
}

describe("the create journey", () => {
  test("four questions land on a provision effect", () => {
    const state = walkCreate();
    expect(state.step).toBe("provisioning");

    const [effect] = takeOnboardingEffect(state);
    expect(effect).toMatchObject({
      kind: "provision",
      choice: "create",
      passphrase: PASSPHRASE,
      relayUrl: "wss://relay.example",
      communityName: "buzz-dev",
      // A create carries no key material: the daemon mints it.
      secret: "",
      unlock: "",
    });
  });

  test("the effect drains exactly once", () => {
    // Same discipline as `takeEffect` in `app/state.ts`, and for the same
    // reason: a second drain would run scrypt twice and try to provision an
    // identity the first call already wrote, failing as "already provisioned".
    const state = walkCreate();
    const [first, cleared] = takeOnboardingEffect(state);
    const [second] = takeOnboardingEffect(cleared);
    expect(first).not.toBeNull();
    expect(second).toBeNull();
  });

  test("create skips the import-only steps", () => {
    let state = initialOnboarding();
    state = applyOnboardingKey(state, ENTER);
    state = type(state, "wss://r.example");
    state = applyOnboardingKey(state, ENTER);
    // choiceIndex 0 is create; enter goes straight to the passphrase, not to
    // "paste your key" — an import field on a create path is a question with no
    // right answer.
    state = applyOnboardingKey(state, ENTER);
    expect(state.step).toBe("passphrase");
  });
});

describe("the import journey", () => {
  test("import routes through the key and unlock steps", () => {
    let state = initialOnboarding();
    state = applyOnboardingKey(state, ENTER);
    state = type(state, "wss://r.example");
    state = applyOnboardingKey(state, ENTER);
    state = applyOnboardingKey(state, DOWN); // select import
    state = applyOnboardingKey(state, ENTER);
    expect(state.step).toBe("identityImport");

    state = type(state, "some-key-material");
    state = applyOnboardingKey(state, ENTER);
    expect(state.step).toBe("importUnlock");

    // Empty is legitimate — a plain secret key has no unlock passphrase — so
    // enter continues rather than rejecting.
    state = applyOnboardingKey(state, ENTER);
    expect(state.step).toBe("passphrase");

    state = type(state, PASSPHRASE);
    state = applyOnboardingKey(state, ENTER);
    state = type(state, PASSPHRASE);
    state = applyOnboardingKey(state, ENTER);
    state = type(state, "buzz-dev");
    state = applyOnboardingKey(state, ENTER);

    const [effect] = takeOnboardingEffect(state);
    expect(effect).toMatchObject({
      choice: "import",
      secret: "some-key-material",
      unlock: "",
    });
  });

  test("an empty paste is refused with an actionable message", () => {
    let state = initialOnboarding();
    state = applyOnboardingKey(state, ENTER);
    state = type(state, "wss://r.example");
    state = applyOnboardingKey(state, ENTER);
    state = applyOnboardingKey(state, DOWN);
    state = applyOnboardingKey(state, ENTER);
    state = applyOnboardingKey(state, ENTER);
    expect(state.step).toBe("identityImport");
    expect(state.error).toContain("esc to go back");
  });
});

describe("validation", () => {
  test("a relay URL needs a websocket scheme and a host", () => {
    expect(validateRelay("")).toContain("required");
    expect(validateRelay("relay.example")).toContain("wss://");
    expect(validateRelay("https://relay.example")).toContain("wss://");
    expect(validateRelay("wss://")).toContain("no host");
    expect(validateRelay("wss://relay.example")).toBeNull();
    // A local relay over plain ws is the `just relay` development case, so it
    // is allowed rather than lectured at.
    expect(validateRelay("ws://localhost:3000")).toBeNull();
  });

  test("the passphrase floor matches the daemon's own", () => {
    // Read out of `provision.rs` rather than restated: two floors that drift
    // produce a wizard that accepts a passphrase the daemon then rejects,
    // *after* the operator has typed it twice.
    const source = require("node:fs").readFileSync(
      new URL("../../../crates/buzz-daemon/src/provision.rs", import.meta.url)
        .pathname,
      "utf8",
    ) as string;
    const match = source.match(/MIN_PASSPHRASE_LEN:\s*usize\s*=\s*(\d+)/);
    expect(match?.[1]).toBeDefined();
    expect(Number(match?.[1])).toBe(MIN_PASSPHRASE_LEN);
  });

  test("a short passphrase is refused before the confirm step", () => {
    expect(validatePassphrase("short")).toContain(String(MIN_PASSPHRASE_LEN));
    expect(validatePassphrase(PASSPHRASE)).toBeNull();
  });

  test("a mismatched confirmation re-asks from the first entry", () => {
    let state = initialOnboarding();
    state = applyOnboardingKey(state, ENTER);
    state = type(state, "wss://r.example");
    state = applyOnboardingKey(state, ENTER);
    state = applyOnboardingKey(state, ENTER);
    state = type(state, PASSPHRASE);
    state = applyOnboardingKey(state, ENTER);
    state = type(state, "something else entirely");
    state = applyOnboardingKey(state, ENTER);

    // Back to `passphrase`, not to a retype of the confirmation: if the two
    // disagree the *first* is as likely to be the typo, and confirming a typo
    // twice is how an account is lost.
    expect(state.step).toBe("passphrase");
    expect(state.secret.passphrase).toBe("");
    expect(state.error).toContain("do not match");
  });
});

describe("escape walks back, and clears what it leaves", () => {
  test("the back graph has no dead ends before provisioning", () => {
    for (const step of [
      "relay",
      "identityChoice",
      "identityImport",
      "importUnlock",
      "passphrase",
      "passphraseConfirm",
      "community",
    ] as const) {
      const state = { ...initialOnboarding(), step };
      expect(previousStep(state)).not.toBeNull();
    }
    // …and the two that must not have one.
    expect(
      previousStep({ ...initialOnboarding(), step: "welcome" }),
    ).toBeNull();
    // Provisioning is mid-scrypt; cancelling would leave a half-written
    // identity directory the next run refuses as "already provisioned".
    expect(
      previousStep({ ...initialOnboarding(), step: "provisioning" }),
    ).toBeNull();
  });

  test("escape from a secret field wipes it", () => {
    let state = initialOnboarding();
    state = applyOnboardingKey(state, ENTER);
    state = type(state, "wss://r.example");
    state = applyOnboardingKey(state, ENTER);
    state = applyOnboardingKey(state, ENTER);
    state = type(state, PASSPHRASE);
    state = applyOnboardingKey(state, ENTER); // stores it, → confirm
    expect(state.secret.passphrase).toBe(PASSPHRASE);

    state = applyOnboardingKey(state, ESC); // back to passphrase
    // Keeping it would leave a passphrase live for a step the operator
    // explicitly left, and would silently pre-fill a field rendering as empty.
    expect(state.secret.passphrase).toBe("");
  });

  test("escape from an import path returns through unlock, not to the choice", () => {
    // The back route differs by branch, and getting it wrong strands an
    // importer on a screen they never visited.
    const importing = {
      ...initialOnboarding(),
      step: "passphrase" as const,
      choice: "import" as const,
    };
    expect(previousStep(importing)).toBe("importUnlock");
    const creating = { ...initialOnboarding(), step: "passphrase" as const };
    expect(previousStep(creating)).toBe("identityChoice");
  });
});

describe("nothing secret is stored in the visible field or rendered back", () => {
  test("every secret step is masked in the frame", () => {
    for (const step of [
      "identityImport",
      "importUnlock",
      "passphrase",
      "passphraseConfirm",
    ] as const) {
      expect(isSecretStep(step)).toBe(true);
      const state = { ...initialOnboarding(), step, input: PASSPHRASE };
      const frame = renderOnboarding(state, 80, 24).join("\n");
      // A terminal is a log with a scrollback buffer, and `capture-pane` is a
      // grep over it — so this is the same rule §2.5 applies to log sinks.
      expect(frame).not.toContain(PASSPHRASE);
      // …but the keystrokes are visibly landing, one mask glyph each.
      expect(frame).toContain("•".repeat(PASSPHRASE.length));
    }
  });

  test("non-secret steps render their value, so the operator can proofread", () => {
    const state = {
      ...initialOnboarding(),
      step: "relay" as const,
      input: "wss://relay.example",
    };
    expect(renderOnboarding(state, 80, 24).join("\n")).toContain(
      "wss://relay.example",
    );
  });

  test("success wipes every secret the flow held", () => {
    const done = onboardingSucceeded({
      ...initialOnboarding(),
      secret: { imported: "k", passphrase: "p", unlock: "u" },
    });
    expect(done.step).toBe("done");
    expect(done.secret).toEqual({ imported: "", passphrase: "", unlock: "" });
  });
});

describe("a failed provision lands on the field that can fix it", () => {
  test("a missing backup passphrase routes to the unlock step", () => {
    const state = onboardingFailed(
      { ...initialOnboarding(), step: "provisioning" },
      "that is an encrypted backup; its own passphrase is needed to open it",
    );
    expect(state.step).toBe("importUnlock");
    expect(state.error).toContain("passphrase is needed");
  });

  test("an unrecognized key routes back to the paste field", () => {
    const state = onboardingFailed(
      { ...initialOnboarding(), step: "provisioning" },
      "unrecognized key: expected …",
    );
    expect(state.step).toBe("identityImport");
  });

  test("an already-provisioned identity routes to the choice", () => {
    const state = onboardingFailed(
      { ...initialOnboarding(), step: "provisioning" },
      "an identity for abcd1234 is already provisioned at /x",
    );
    expect(state.step).toBe("identityChoice");
  });

  test("an unattributable failure keeps the daemon's own words", () => {
    // Reshaping it into a guess about which step was wrong would be worse than
    // the raw message: the operator can at least search for the raw one.
    const state = onboardingFailed(
      { ...initialOnboarding(), step: "provisioning" },
      "no space left on device",
    );
    expect(state.error).toBe("no space left on device");
  });
});

describe("the frame", () => {
  test("every step renders exactly the requested geometry", () => {
    // Same contract `renderScreen` holds: a short frame leaves the previous
    // paint's rows on screen and a tall one pushes the field off the bottom.
    for (const step of [
      "welcome",
      "relay",
      "identityChoice",
      "identityImport",
      "importUnlock",
      "passphrase",
      "passphraseConfirm",
      "community",
      "provisioning",
      "done",
    ] as const) {
      for (const [cols, rows] of [
        [80, 24],
        [40, 16],
        [120, 40],
      ] as const) {
        const frame = renderOnboarding(
          { ...initialOnboarding(), step },
          cols,
          rows,
        );
        expect(frame).toHaveLength(rows);
        for (const line of frame) {
          expect(line.length).toBeLessThanOrEqual(cols * 2);
        }
      }
    }
  });

  test("[G8] holds: exactly one focus glyph on a field step, none without", () => {
    const withField = renderOnboarding(
      { ...initialOnboarding(), step: "relay" },
      80,
      24,
    ).join("\n");
    expect(withField.split("❯").length - 1).toBe(1);

    // `identityChoice` has no field; the glyph marks the selected row instead —
    // still exactly one, which is the invariant that actually matters.
    const choice = renderOnboarding(
      { ...initialOnboarding(), step: "identityChoice" },
      80,
      24,
    ).join("\n");
    expect(choice.split("❯").length - 1).toBe(1);
  });

  test("a fieldless step carries no placeholder", () => {
    // Regression, found by a PTY run. `welcome` carried "press enter to begin"
    // as a placeholder, but `welcome` renders no field — so the string was
    // copy that read as live in the source and appeared nowhere on screen.
    // Dead copy in a wizard is worse than missing copy: a reviewer reads it,
    // believes the affordance is present, and stops looking.
    //
    // The affordance lives in `hints`, which does render — so that is asserted
    // here too rather than only the absence.
    for (const step of [
      "welcome",
      "identityChoice",
      "provisioning",
      "done",
    ] as const) {
      expect(promptFor(step).placeholder).toBe("");
      const frame = renderOnboarding(
        { ...initialOnboarding(), step },
        80,
        24,
      ).join("\n");
      expect(frame).not.toContain("press enter");
    }
    expect(promptFor("welcome").hints).toContain("⏎ begin");
    expect(
      renderOnboarding({ ...initialOnboarding() }, 80, 24).join("\n"),
    ).toContain("⏎ begin");
  });

  test("the welcome screen says what happens to the key", () => {
    // The single most important sentence in the product's first impression,
    // and the one a copy edit is most likely to shorten away.
    const welcome = promptFor("welcome");
    const body = welcome.body.join(" ");
    expect(body).toContain("locally");
    expect(body).toContain("never sent anywhere");
  });

  test("the passphrase screen says there is no reset", () => {
    const body = promptFor("passphrase").body.join(" ");
    expect(body).toContain("no reset");
  });
});
