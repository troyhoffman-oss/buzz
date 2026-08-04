/**
 * Compile `buzz-tui` to a single binary — DESIGN.md §6.2(b), §6.3.
 *
 * Uses `Bun.build` with `@opentui/solid/bun-plugin` rather than the plain
 * `bun build --compile` CLI, because **the CLI does not apply the Solid
 * transform**: `bunfig.toml`'s `preload` is a runtime hook and is not consulted
 * during compilation, so a CLI-compiled binary ships untransformed JSX and
 * fails at first render with the same "Orphan text error" a missing preload
 * produces at dev time. Upstream documents this in
 * `@opentui/solid`'s README (anomalyco/opentui#122).
 *
 * Usage: `bun run scripts/build.ts <bun-target-triple> <output-name>`
 *
 * §6.3's matrix is `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, and
 * `x86_64-apple-darwin` [LOCKED]. The Rust triples are the *artifact* names;
 * Bun's own target strings are the `bun-<os>-<arch>` forms mapped below, so the
 * released filenames match the daemon's and a user does not have to translate
 * between two naming schemes.
 */

import solidPlugin from "@opentui/solid/bun-plugin";

/**
 * Rust artifact triple → Bun compile target (§6.3).
 *
 * `as const` matters: without it the lookup widens to `string` and Bun's
 * `CompileTarget` union rejects it, so the mapping would only be validated at
 * runtime on a release tag — the worst place to discover a typo.
 */
const TARGETS = {
  "x86_64-unknown-linux-gnu": "bun-linux-x64",
  "aarch64-apple-darwin": "bun-darwin-arm64",
  "x86_64-apple-darwin": "bun-darwin-x64",
} as const;

/** A supported release triple (§6.3's LOCKED matrix). */
type Triple = keyof typeof TARGETS;

function isTriple(value: string): value is Triple {
  return value in TARGETS;
}

const requested = process.argv[2];
if (!requested || !isTriple(requested)) {
  console.error(
    `::error::usage: bun run scripts/build.ts <triple>\n` +
      `triples: ${Object.keys(TARGETS).join(", ")}`,
  );
  process.exit(1);
}

const target = TARGETS[requested];
const outfile = `dist/buzz-tui-${requested}`;

const result = await Bun.build({
  entrypoints: ["./src/main.ts"],
  target: "bun",
  plugins: [solidPlugin],
  compile: { target, outfile },
});

if (!result.success) {
  for (const log of result.logs) console.error(log);
  process.exit(1);
}

console.log(`built ${outfile} (${target})`);
