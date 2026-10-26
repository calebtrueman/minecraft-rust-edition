# Contributing to Minecraft: Rust Edition

This is an unofficial Rust game and launcher forked from Horizon. The game engine stays in
Rust. Java tools may prepare files from the player's own installation or serve as a reference
in tests. Do not add a Java-game fallback and call it Rust gameplay.

The fork is still unfinished. Existing Fabric mods do not run in the game yet. Read
[the current status](README.md) and [the mod compatibility guide](docs/java-mod-compatibility.md)
before making claims about what works.

## Licenses and credit

Open-source libraries and changes to them are welcome when their licenses allow our use.
The upstream project's restriction against using other projects' code does not apply to this fork.

- Keep each library's license and required notices. Do not remove an author's credit.
- Record where added or copied code came from, its version, and what we changed.
- Prefer a normal package dependency. If we keep a patched copy in the repository, include
  its license and a short note explaining the patch.
- Do not include code with an unknown or incompatible license.
- Keep the original Horizon MIT license and copyright notice in [LICENSE](LICENSE).

Minecraft's game jars, assets, decompiled source, prepared files, account sessions and private
keys must stay outside Git. Read the player's files without changing the source installation.
Generated test data belongs in ignored folders. `tools/check_public_tree.py` checks file paths;
it does not replace a license review.

## Build and test

See [source builds](docs/source-builds.md) for the local installer and build commands. The
launcher and the early Java-code runtime are separate Cargo workspaces. The `AGENTS.md` map
lists the game code and the checks for each area.

Use fresh test folders for every game or launcher run. Set `HORIZON_HOME`,
`HORIZON_LAUNCHER_HOME`, `XDG_DATA_HOME` and `XDG_CONFIG_HOME` to those folders. Set
`HORIZON_LAUNCHER_NO_SELF_UPDATE=1` and `HORIZON_NO_AUDIO=1`. Tests must not change real saves,
launcher settings or installed applications. Use fake local services when real account access
is not needed.

For a code change, run the matching formatting, compiler, lint and behavior checks. Use the
pinned Java game as a reference where behavior matters. Keep reference game files local.
For Java-code runtime tests, use small original Java programs and compare their results with
Rust. Do not mistake a tiny example for support for a real mod.

Describe what was checked and what was not. An offline test does not prove account login.
A successful server join does not prove every multiplayer feature. A successful build does
not prove the app looks right. Do not start browser checks or wait on deployments unless
that work was requested.

## Commits and reviews

Keep changes focused and explain why they are needed in plain words. Follow the existing
commit title style, such as `Launcher:`, `Multiplayer:`, `Mods:` or `Build:`.
Fetch and merge remote changes before committing or pushing. Include the actual AI model
and reasoning setting in its co-author line when an AI tool helped; do not copy an example
model name without checking it.

The manual **Rust Edition source builds** workflow makes test artifacts on GitHub-hosted
runners. It does not publish releases. A published release needs this fork's signing setup;
never use the upstream author's private machines, keys or account registrations.

If code changes a database schema, confirm that the production schema is ready before
pushing code that uses it. Nothing in a green local test overrides that check.
