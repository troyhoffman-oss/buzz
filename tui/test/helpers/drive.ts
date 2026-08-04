/**
 * Drive the app with key sequences — the harness NAVIGATION.md §4's four
 * walkthroughs are written against.
 *
 * The whole navigation model is pure (`nav/keys.ts` resolves, `app/dispatch.ts`
 * reduces, `app/screen.ts` renders), so a walkthrough test is literally the
 * key sequence from the spec plus assertions on the resulting rows. No
 * terminal, no daemon, no timers.
 */

import { applyIntent, withDefaultSelection } from "../../src/app/dispatch";
import { renderScreen, rowContext } from "../../src/app/screen";
import {
  type AppState,
  type PendingEffect,
  initialState,
  takeEffect,
} from "../../src/app/state";
import { FixtureClient } from "../../src/client/fixture-client";
import { type KeyPress, resolveKey } from "../../src/nav/keys";
import { breadcrumb, current } from "../../src/nav/layers";

/**
 * The frozen clock every test renders against — §5.3 requirement 1.
 *
 * Matches `scripts/gen-fixtures.ts`'s `T0`, so a fixture authored as
 * `T0 - 3 * MIN` renders as `14:09` in every assertion below.
 */
export const FIXED_NOW = Date.parse("2026-08-04T14:12:00.000Z");

/** A driven session: state plus the sugar for pressing keys and reading rows. */
export class Session {
  state: AppState;
  constructor(
    readonly client: FixtureClient,
    public cols = 120,
    public rows = 30,
  ) {
    this.state = withDefaultSelection(initialState(client.getSnapshot()));
  }

  /** Load a scenario by name from `fixtures/`. */
  static open(scenario: string, cols = 120, rows = 30): Session {
    return new Session(FixtureClient.fromFile(scenario), cols, rows);
  }

  /** Press one key, resolving it through the §5 chains exactly as the Shell does. */
  press(key: KeyPress): this {
    const s = this.state;
    const intent = resolveKey(
      {
        layer: current(s.stack).kind,
        composer: {
          text: s.composer,
          cursor: s.cursor,
          multiline: false,
          cursorRow: 0,
          rowCount: 1,
        },
        surfaces: s.surfaces,
        drawerOpen: s.drawer !== null && !s.drawer.expanded,
        drawerExpanded: s.drawer?.expanded ?? false,
        messageSelect: s.messageSelect !== null,
        // The same derivation the Shell uses, so a walkthrough exercises the
        // real §5.1 rows 3 and 5 rather than a test-only shortcut.
        ...rowContext(s),
      },
      key,
    );
    const next = applyIntent(this.state, intent, this.cols, FIXED_NOW);
    // **Drain effects exactly as `Shell.tsx` does.** A harness that leaves
    // `pending` set is not driving the app, it is driving the reducer: the
    // whole write path (`⏎` to send, `markRead`) can be dead and every
    // walkthrough here still passes, because the reducer's half — clearing the
    // composer — is the visible half. That is precisely how the missing
    // `takeEffect` call in the Shell survived 374 green tests.
    const [effect, cleared] = takeEffect(next);
    this.state = cleared;
    if (effect) this.effects.push(effect);
    return this;
  }

  /**
   * Effects the reducer emitted, in order — what the Shell would have sent.
   *
   * Recorded rather than performed: the fixture client's `send` is async and a
   * synchronous `press()` cannot await it, and asserting on the *decision* is
   * the stronger test anyway ([D-2]'s "what you picked is what gets tagged" is
   * a property of this array, not of the daemon's reply).
   */
  readonly effects: PendingEffect[] = [];

  /** Press a named key with no modifiers. */
  key(name: string, modifiers: Omit<KeyPress, "name"> = {}): this {
    return this.press({ name, ...modifiers });
  }

  /** Type a string, one printable at a time. */
  type(text: string): this {
    for (const char of text) this.press({ name: char, char });
    return this;
  }

  /** The rendered screen. */
  screen(): string[] {
    return renderScreen(this.state, this.cols, this.rows, FIXED_NOW);
  }

  /** The rendered screen as one string, for `toContain` assertions. */
  text(): string {
    return this.screen().join("\n");
  }

  /** The current breadcrumb — what §4.4 asserts about the path taken. */
  crumb(): string {
    return breadcrumb(this.state.stack);
  }

  /** The current layer's kind. */
  layer(): string {
    return current(this.state.stack).kind;
  }

  /** The row carrying `❯` — [G8]'s one focus glyph. */
  focusRow(): string | undefined {
    return this.screen().find((row) => row.includes("❯"));
  }

  /** How many `❯` glyphs are on screen. Must always be exactly one [G8]. */
  focusGlyphCount(): number {
    return this.screen().reduce(
      (sum, row) => sum + (row.match(/❯/g)?.length ?? 0),
      0,
    );
  }

  /**
   * Walk `⇧↓`/`↓` until `❯` is on a row containing `label`.
   *
   * Tests navigate by *destination*, never by a hardcoded press count. §1.1's
   * `⇧↓` steps one structural boundary at a time, and home's boundary count is
   * a function of which ATTENTION groups have items — so "three `⇧↓` presses
   * reach PLACES" is true of one fixture and false of the next. A test that
   * encoded the count would fail when a fixture gained a group, reporting a
   * navigation bug where there is only a different list.
   */
  goTo(label: string, limit = 40): this {
    for (let i = 0; i < limit; i++) {
      if (this.focusRow()?.includes(label)) return this;
      const before = this.focusRow();
      this.key("down");
      if (this.focusRow() === before) break;
    }
    throw new Error(
      `goTo(${label}) never reached the row; focus is on ${this.focusRow()?.trim()}`,
    );
  }

  /** Resize, then re-render — §7's reflow, not a tier change. */
  resize(cols: number, rows: number): this {
    this.cols = cols;
    this.rows = rows;
    return this;
  }

  /** Advance the fixture clock and refresh the snapshot. */
  advance(ms: number): this {
    this.client.advanceTo(ms);
    this.state = { ...this.state, snapshot: this.client.getSnapshot() };
    return this;
  }
}
