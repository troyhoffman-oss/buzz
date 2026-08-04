/**
 * Placeholder for the T2 tmux-drive suite — DESIGN.md §5.5.
 *
 * Exists for the same reason as `test/render/harness.test.ts`: an empty
 * directory exits 0, so an unwritten suite and a passing suite are
 * indistinguishable in CI.
 *
 * `scripts/smoke.sh` (`just tui-smoke`) is the one T2-shaped check that runs
 * today: it boots the shell in a real PTY, asserts the pinned pane geometry and
 * every §3.0 region, and asserts a clean quit on `q`. The sequences below are
 * §5.5's table.
 */

import { test } from "bun:test";

test.todo("type @ma: popup opens, @matt first, extmark styling applied", () => {});
test.todo("ctrl+c mid-compose clears the composer and does not exit on first press", () => {});
test.todo("two TUIs within 50ms yield exactly one daemon pid (§2.3)", () => {});
test.todo("resize 120->72->50->120 walks MD -> MD-narrow -> XS -> MD", () => {});
test.todo("unread divider anchored to an event id survives a burst", () => {});
test.todo("killing the fixture transport shows reconnecting + countdown, not silence", () => {});
test.todo("a draft composed in pane 1 appears in pane 2 (drafts are daemon-held)", () => {});
