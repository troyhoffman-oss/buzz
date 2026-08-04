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
  # Character-wise scan rather than a regex sub. A naive `sub(/\/\/.*$/, "")`
  # truncates at the `//` inside a URL string — so a violation on the same line
  # as, or *after*, any `https://` literal silently disappears. That is a
  # false negative in a security-shaped gate, which is the one direction a gate
  # must never fail.
  #
  # String contents are preserved rather than blanked: a kind embedded in a
  # string is still a kind.
  awk '
    {
      line = $0
      out = ""
      i = 1
      n = length(line)
      while (i <= n) {
        c = substr(line, i, 1)
        two = substr(line, i, 2)
        if (inblock) {
          if (two == "*/") { inblock = 0; i += 2 } else { i++ }
          continue
        }
        if (instr) {
          out = out c
          if (c == "\\") { out = out substr(line, i + 1, 1); i += 2; continue }
          if (c == quote) instr = 0
          i++
          continue
        }
        if (c == "\"" || c == "'"'"'" || c == "`") { instr = 1; quote = c; out = out c; i++; continue }
        if (two == "/*") { inblock = 1; i += 2; continue }
        if (two == "//") { break }
        out = out c
        i++
      }
      # A template literal may span lines; reset at end of line only when not
      # inside one, so its contents keep being scanned.
      if (instr && quote != "`") instr = 0
      print out
    }
  ' "$file" > "$CODE_ONLY/$file"
done < <(find "$SRC" -type f \( -name '*.ts' -o -name '*.tsx' \))

scan() {
  # $1 = extended regex, printed with real paths rather than the temp dir.
  (cd "$CODE_ONLY" && grep -rnE "$1" "$SRC" 2>/dev/null) || return 1
}

# 1. No protocol dependency may be declared. The daemon owns NIP-44, bech32,
#    secp256k1, and every nsec/npub decode; the TUI never sees a key (§2.1).
PROTOCOL_PKGS='nostr nostr-tools secp256k1 @noble/secp256k1 @noble/curves @scure/base bech32 nip44 nip-44 nsecs'
for pkg in $PROTOCOL_PKGS; do
  if grep -q "\"$pkg\"" package.json; then
    fail "package.json declares '$pkg': protocol dependencies belong in crates/buzz-daemon (§6.4)"
  fi
done

# 1b. …and no module in `src/` may *import* one either. Scanning only
#     `package.json` is a false negative in the direction that matters: a
#     transitive dependency, a workspace link, or a hoisted `node_modules`
#     entry is importable without ever being declared, so `import { getPublicKey }
#     from "nostr-tools"` passed the manifest check untouched. §6.4 is about what
#     `tui/src/` *knows*, and an import is exactly that knowledge — the manifest
#     is only where it usually comes from.
for pkg in $PROTOCOL_PKGS; do
  # Match the package root and its subpaths ("nostr-tools/pure"), not a
  # same-prefixed unrelated package ("nostrich-ui").
  if scan "from[[:space:]]+['\"\`]${pkg}(/|['\"\`])|require\([[:space:]]*['\"\`]${pkg}(/|['\"\`])|import\([[:space:]]*['\"\`]${pkg}(/|['\"\`])"; then
    fail "$SRC/ imports '$pkg': protocol dependencies belong in crates/buzz-daemon (§6.4)"
  fi
done

# 2. No bare event-kind integer in the 4-digit-and-up range (§6.4, verbatim).
#    Kinds are daemon vocabulary — the TUI receives `type: "message.new"`,
#    never `kind: 40002`.
#
#    Deliberately NOT narrowed to `kind`-adjacent numbers. That reading passes
#    `const OBSERVER_FRAME = 24200` and even `kind2 = 40002` (a `\b` after
#    `kind` requires a non-word character, and `2` is one), which is precisely
#    the leak the gate exists to stop. A blanket 4-digit rule is what §6.4
#    actually specifies, and it is the safer default: a genuine non-kind
#    constant of that size is rare in a front end and can be allowlisted
#    explicitly below, whereas a missed kind is silent.
#
#    Allowlisted, with a reason each:
#      - the timing/geometry literals the shell legitimately carries (none yet);
#      - anything inside a string that is obviously not a kind is NOT
#        allowlisted, because a kind in a string is still a kind.
if scan '(^|[^0-9a-zA-Z_.])[0-9]{4,}'; then
  fail "a bare 4-digit-or-longer integer appears in $SRC/: event kinds are daemon vocabulary (§6.4)"
fi

# 3. No cursor decoding. [D-6] makes cursors decodable for humans debugging,
#    not for the client — the TUI treats them as opaque.
if scan 'atob\(|Buffer\.from\([^)]*base64|fromBase64|c1\.'; then
  fail "$SRC/ decodes a pagination cursor: cursors are opaque to the front end (§2.4 [D-6])"
fi

# 4. No `{nsec}` field, and no raw key material of any shape. §2.5 deleted the
#    `{nsec}` form from POST /session/identity; a regenerated client that grows
#    one is a spec regression, not a convenience.
#
#    The trailing `\b` here used to be a false negative on the single most
#    important case: `\bnsec\b` does NOT match `nsec1qqq…`, because `nsec` is
#    followed by `1` — a word character — so there is no boundary. The gate
#    matched the bare identifier `nsec` while an actual bech32 secret key
#    literal pasted into `src/` sailed through. Anchor the left edge only, and
#    cover the encrypted form too, since §2.5 puts `ncryptsec1` in the daemon's
#    redactor superset for exactly this reason.
if scan '\b(nsec|ncryptsec)'; then
  fail "$SRC/ references a raw nsec/ncryptsec: the {nsec} request form does not exist and key material never reaches the front end (§2.5)"
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
