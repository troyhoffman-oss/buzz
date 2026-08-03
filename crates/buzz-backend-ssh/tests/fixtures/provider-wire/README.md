# Provider wire fixtures

Byte-for-byte copies of the golden fixtures upstream ships at
`crates/buzz-backend-kubernetes/tests/fixtures/provider-wire/`, which are the
shared arbiter for the stdin/stdout contract between the desktop
(`commands/agents_deploy.rs`) and any provider (spec §Provider Protocol).

They are copied rather than referenced because this crate ships and updates
independently of the desktop that fills the payload — a path reference would
make these tests pass or fail based on which checkout they happen to run in,
which is the opposite of what a pinned contract is for.

`deploy-full-launch.request.json` is the important one: upstream's README
records that it is *executed and transcribed* output of the desktop's real
`build_launch_block` → `deploy_payload_json` path, not a shape someone derived
by reading those functions. That provenance is the whole value. In particular
it is why `agent_args` is `[]` while `launch.args` is `["acp"]` — a provider
that read the legacy field would launch Goose with no subcommand, and no
invented fixture would have caught it.

Only the `provider_config` block is provider-specific; the tests substitute an
SSH one and leave the `agent` block untouched, so a drift in the desktop's
emitted payload shows up here as a diff.

To refresh:

    git show origin/main:crates/buzz-backend-kubernetes/tests/fixtures/provider-wire/<file> \
      > crates/buzz-backend-ssh/tests/fixtures/provider-wire/<file>
