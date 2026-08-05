/**
 * Running the wizard's provision effect against `buzz-daemon`.
 *
 * The impure half of onboarding, kept in its own file so `flow.ts` stays a pure
 * reducer. Everything secret crosses exactly one boundary here — the child's
 * **stdin** — and this module is the only place in `tui/src/` that holds a
 * passphrase for longer than a keystroke.
 *
 * # Why a subprocess rather than an endpoint
 *
 * §2.5 rules on it directly: "If a raw-nsec import is ever needed for
 * onboarding, it is `buzz-tui identity import` — one shot, writes an ncryptsec
 * to disk, then uses path 1 — **not a daemon endpoint**." A first-run flow has
 * no daemon to POST to anyway: the daemon cannot start without the identity
 * this flow creates.
 */

import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import type { ProvisionEffect } from "./flow";
import type { TuiConfig } from "../startup/paths";

/** What provisioning produced. Mirrors the daemon's `ProvisionOutcome`. */
export interface ProvisionResult {
  readonly pubkey: string;
  readonly created: boolean;
}

/** Where a provisioned identity's daemon should listen. */
export interface SocketPaths {
  readonly socket: string;
  readonly lock: string;
}

/** A provisioning failure carrying the daemon's own message. */
export class ProvisionFailed extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProvisionFailed";
  }
}

/**
 * Run `buzz-daemon identity provision`, feeding the request on stdin.
 *
 * The request JSON carries the passphrase and (for an import) the key, so it
 * **must not** become an argument or an environment variable: `/proc/<pid>/cmdline`
 * is world-readable, and §1.2's own premise is a box running arbitrary agents
 * under other uids.
 */
export async function runProvision(
  binary: string,
  effect: ProvisionEffect,
): Promise<ProvisionResult> {
  const request =
    effect.choice === "create"
      ? { mode: "create", passphrase: effect.passphrase }
      : {
          mode: "import",
          secret: effect.secret,
          passphrase: effect.passphrase,
          // Omitted rather than sent empty: the daemon's `unlock` is
          // `Option<String>`, and an empty string would make it try to open a
          // plain nsec as an encrypted blob and report the wrong error.
          ...(effect.unlock.length > 0 ? { unlock: effect.unlock } : {}),
        };

  const child = Bun.spawn([binary, "identity", "provision"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  child.stdin.write(`${JSON.stringify(request)}\n`);
  await child.stdin.end();

  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);

  if (code !== 0) {
    throw new ProvisionFailed(
      cleanError(stderr) || `provisioning failed (${code})`,
    );
  }
  const parsed = parseJsonLine(stdout);
  const pubkey = typeof parsed?.pubkey === "string" ? parsed.pubkey : "";
  if (!pubkey) {
    throw new ProvisionFailed("the daemon did not report a pubkey");
  }
  return { pubkey, created: parsed?.created === true };
}

/**
 * Ask the daemon where this (relay, identity) pair's socket lives.
 *
 * §2.2's hash is not reimplemented in the TUI — see `startup/paths.ts`. This is
 * the one call that answers it, and it costs no scrypt, so it runs on every
 * launch rather than being cached into a config that could go stale against a
 * changed auth tag.
 */
export async function resolveSocketPaths(
  binary: string,
  relayUrl: string,
  pubkey: string,
): Promise<SocketPaths> {
  const child = Bun.spawn(
    [binary, "socket-path", "--relay", relayUrl, "--identity", pubkey],
    { stdout: "pipe", stderr: "pipe" },
  );
  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (code !== 0) {
    throw new ProvisionFailed(
      cleanError(stderr) || "could not resolve the daemon socket path",
    );
  }
  const parsed = parseJsonLine(stdout);
  const socket = typeof parsed?.socket === "string" ? parsed.socket : "";
  const lock = typeof parsed?.lock === "string" ? parsed.lock : "";
  if (!socket || !lock) {
    throw new ProvisionFailed("the daemon did not report a socket path");
  }
  return { socket, lock };
}

/** List the identities already provisioned on this machine. */
export async function listIdentities(binary: string): Promise<string[]> {
  const child = Bun.spawn([binary, "identity", "list"], {
    stdout: "pipe",
    stderr: "ignore",
  });
  const [stdout, code] = await Promise.all([
    new Response(child.stdout).text(),
    child.exited,
  ]);
  if (code !== 0) return [];
  const parsed = parseJsonLine(stdout);
  const list = parsed?.identities;
  return Array.isArray(list)
    ? list.filter((v): v is string => typeof v === "string")
    : [];
}

/**
 * Persist the TUI's own config after a successful onboarding.
 *
 * `0600` because the file names an identity and a relay — not secrets, but a
 * complete description of who this operator is, and the directory it sits in is
 * `0700` for the same reason the daemon's is.
 *
 * The **socket path is deliberately not written**. It is re-derived on every
 * launch through {@link resolveSocketPaths}, because the preimage includes the
 * NIP-OA owner (§2.2) and an auth tag added later changes the answer. A cached
 * path would silently point at the untagged daemon after the operator gained a
 * tag — two effective identities, one cache, which is the collision §2.2 names.
 */
export function writeConfig(path: string, config: TuiConfig): void {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  writeFileSync(
    path,
    `${JSON.stringify(
      {
        relayUrl: config.relayUrl,
        pubkey: config.pubkey,
        communityName: config.communityName,
      },
      null,
      2,
    )}\n`,
    { mode: 0o600 },
  );
  // `writeFileSync`'s `mode` applies only at create, so an existing file keeps
  // whatever mode it had. Re-running onboarding over a `0644` config left by an
  // older build would otherwise silently keep it world-readable.
  chmodSync(path, 0o600);
}

/**
 * The daemon's stderr, reduced to the line worth showing.
 *
 * `tracing` writes structured lines and `clap` writes usage blocks; the actual
 * error is the last non-empty line in both cases. Showing the whole buffer
 * would put a log dump in a wizard, and showing the first line would show a
 * banner.
 */
function cleanError(stderr: string): string {
  const lines = stderr
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
  const last = lines.at(-1) ?? "";
  // `Error: "…"` is how `Box<dyn Error>` prints a string error from `main`.
  // Unwrapping it is the difference between a sentence and a debug rendering.
  const unwrapped = last.replace(/^Error:\s*/, "").replace(/^"(.*)"$/, "$1");
  return unwrapped;
}

/** Parse the one JSON object a subcommand prints. */
function parseJsonLine(stdout: string): Record<string, unknown> | null {
  const line = stdout
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.startsWith("{"))
    .at(-1);
  if (!line) return null;
  try {
    const parsed: unknown = JSON.parse(line);
    return typeof parsed === "object" && parsed !== null
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}
