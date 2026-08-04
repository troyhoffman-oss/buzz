#!/usr/bin/env bash
# `tui-check-boundary` — enforce "the front end is disposable" (DESIGN.md §6.4).
#
# This is [LOCKED] intent, so it gets a mechanical gate rather than a
# convention. The build fails if `tui/src/` contains any of the things below,
# each of which would move protocol knowledge out of `crates/buzz-daemon` and
# into the disposable half.
#
# The test for whether the design succeeded: a `ratatui` front end can be
# written against the same daemon and lose **zero** protocol work. If any of
# these appears in the TS, that stops being true.
#
# Scope is `src/` only. `test/` and `scripts/` legitimately mention kinds and
# endpoint names in comments and fixtures.
set -euo pipefail

cd "$(dirname "$0")/.."

SRC="src"
failures=0

fail() {
  echo "::error::$1" >&2
  failures=$((failures + 1))
}

# The scan runs over **code with comments stripped**. A doc comment that
# explains this very rule ("never a {nsec} field", "kinds are daemon
# vocabulary") would otherwise trip the gate that the comment documents —
# which pressures the next author to delete the explanation rather than fix a
# violation. Stripping comments keeps the rule and its rationale co-located.
#
# Line numbers are preserved (comment bodies become blank lines) so a violation
# still points at the right line.
CODE_ONLY="$(mktemp -d)"
trap 'rm -rf "$CODE_ONLY"' EXIT

while IFS= read -r file; do
  mkdir -p "$CODE_ONLY/$(dirname "$file")"
  # Blank out /* ... */ blocks and // ... tails, keeping line count intact.
  awk '
    { line = $0 }
    inblock {
      if (match(line, /\*\//)) { line = substr(line, RSTART + 2); inblock = 0 }
      else { print ""; next }
    }
    {
      while (match(line, /\/\*/)) {
        head = substr(line, 1, RSTART - 1)
        rest = substr(line, RSTART + 2)
        if (match(rest, /\*\//)) { line = head substr(rest, RSTART + 2) }
        else { line = head; inblock = 1; break }
      }
      sub(/\/\/.*$/, "", line)
      print line
    }
  ' "$file" > "$CODE_ONLY/$file"
done < <(find "$SRC" -type f \( -name '*.ts' -o -name '*.tsx' \))

scan() {
  # $1 = extended regex, printed with real paths rather than the temp dir.
  (cd "$CODE_ONLY" && grep -rnE "$1" "$SRC" 2>/dev/null) || return 1
}

# 1. No protocol dependency may be declared. The daemon owns NIP-44, bech32,
#    secp256k1, and every nsec/npub decode; the TUI never sees a key (§2.1).
for pkg in nostr nostr-tools secp256k1 @noble/secp256k1 bech32 nip44 nip-44 nsecs; do
  if grep -q "\"$pkg\"" package.json; then
    fail "package.json declares '$pkg': protocol dependencies belong in crates/buzz-daemon (§6.4)"
  fi
done

# 2. No bare event-kind integer in the 4-digit-and-up range. Kinds are daemon
#    vocabulary — the TUI receives `type: "message.new"`, never `kind: 40002`.
#    Matched as `kind`-adjacent so ordinary numbers (widths, timeouts, ports)
#    are not false positives.
if scan '\bkinds?\b[^a-zA-Z]{0,4}[0-9]{4,}'; then
  fail "a bare event-kind integer appears in $SRC/: kinds are daemon vocabulary (§6.4)"
fi

# 3. No cursor decoding. [D-6] makes cursors decodable for humans debugging,
#    not for the client — the TUI treats them as opaque.
if scan 'atob\(|Buffer\.from\([^)]*base64|fromBase64|c1\.'; then
  fail "$SRC/ decodes a pagination cursor: cursors are opaque to the front end (§2.4 [D-6])"
fi

# 4. No `{nsec}` field. §2.5 deleted that form from POST /session/identity; a
#    regenerated client that grows one is a spec regression, not a convenience.
if scan '\bnsec\b'; then
  fail "$SRC/ references a raw nsec: the {nsec} request form does not exist (§2.5)"
fi

# 5. No literal hex colour. §3.10 requires a closed semantic token set with the
#    syntax palette derived, not hand-picked per call site.
if scan '#[0-9a-fA-F]{3,8}\b'; then
  fail "$SRC/ contains a literal hex colour: use a semantic theme token (§3.10)"
fi

if [[ $failures -gt 0 ]]; then
  echo "" >&2
  echo "$failures boundary violation(s). §6.4: a ratatui front end must be able to" >&2
  echo "replace tui/ against the same daemon and lose zero protocol work." >&2
  exit 1
fi

echo "tui-check-boundary: $SRC/ holds no protocol knowledge"
