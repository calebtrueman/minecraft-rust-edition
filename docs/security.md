# Security

This page describes Minecraft: Rust Edition. The original Horizon license and credits remain in
`LICENSE`; the fork has its own downloads, settings folders and build checks.

## Current release status

No signed fork release has been published. The source-build workflow creates test packages on
GitHub-hosted runners. Those artifacts and the launcher's local-folder import are not signed
release downloads. The inherited automatic release workflows are disabled for this fork.

The launcher and server already verify downloaded releases. They check signed `SHA256SUMS`
against public keys in `crates/horizon-release-sign/release-keys.txt`, then check the archive's hash.
The signature covers the release version. An unknown key, changed archive or mismatched version
is refused. A separate table in `legacy-releases.txt` accepts only the exact hashes of listed old,
unsigned upstream releases; it does not make arbitrary unsigned files acceptable.

The committed trust keys and legacy table still come from upstream. Before publishing fork
releases, set up a fork-owned signing key and ship its public key in the launcher and server.
Do not weaken verification to make a release install. An older installed build may need a manual
replacement before it can trust a new key.

## Preparing a signing key

The `release` environment in this fork must exist before running `tools/ci/new_release_key.sh`.
The helper defaults to `calebtrueman/minecraft-rust-edition` and can use `GITHUB_REPOSITORY` for an
explicit target. It pipes a new private key directly to that environment's GitHub secret and prints
only the public key. It is an explicit maintenance command; normal builds do not run it.

Review and commit the public key, then test signing and verification with a fresh tag that is not
in the legacy table. Keep the private key out of Git, logs and build artifacts. Review the signing
and publishing workflow before enabling releases. A valid signature proves who signed an artifact;
it does not prove that the code works correctly.

The first-install server shell and PowerShell scripts currently check GitHub's published hash.
Later server updates use the embedded signature verifier. First-install signature verification
still needs work.

## Player data and network use

- The game prepares resources from the player's own Minecraft files. Minecraft jars, assets,
  credentials and prepared resources stay outside Git and test packages.
- Account import checks Minecraft ownership and the selected profile. It does not import another
  launcher's Microsoft refresh token or app ID. Imported game sessions still expire.
- Telemetry has no default endpoint. Discord presence and mod-registry migration require explicit
  configuration. No upstream registry is fetched automatically.
- Optional registry and telemetry services are not deployed by the source-build workflow. Their
  repository, origin and credentials must be configured before use.

Tests use fresh data folders. The Java/Fabric compatibility work is separate from the game; reading
metadata is not a sandbox for executing mods. Its runtime and game bindings still need work before
installed Java mods can be supported.

## Repository settings and reports

`CODEOWNERS` identifies the fork's owner, but the file alone does not enforce review. Branch rules,
release-environment protections and private vulnerability reporting must be checked in this
repository's GitHub settings; upstream settings do not carry over merely because the code was forked.

Use private vulnerability reporting in the repository's Security tab when available. Do not put
account tokens or other secrets in a public issue.
