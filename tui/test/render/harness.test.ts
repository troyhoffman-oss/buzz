/**
 * Placeholder for the T1 headless-render suite — DESIGN.md §5.3.
 *
 * This file exists so the suite cannot be **green by being empty**. `bun test`
 * on a directory with no matching files exits 0, which makes an unwritten
 * snapshot matrix indistinguishable from a passing one in CI — precisely the
 * silence §1.3 property 3 objects to, applied to the test suite itself.
 *
 * Delete this file when `test/render/` holds the real matrix (see the README
 * beside it for the six determinism requirements and the screen × tier grid).
 */

import { test } from "bun:test";

test.todo("T1 snapshot matrix: every screen x every tier x both glyph policies", () => {});
test.todo("T1 shadow run: two runs of one input produce byte-identical buffers", () => {});
