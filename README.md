# Minecraft: Rust Edition

An unofficial Rust game and launcher, forked from [Horizon](https://github.com/ludovic111/horizon).
Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.
The original MIT license and credits remain in [LICENSE](LICENSE).

## Fork status

This is work in progress, not a finished replacement for Java Edition.

- Gameplay stays in Rust. Java is used to prepare the game files from your own installation.
- The launcher reads the existing Minecraft files it supports. Its current game version is pinned
  in `research-lock.json`; an older Java installation alone is not enough.
- **Use Prism account** imports the selected account's current Minecraft session. Ownership and
  profile are checked with Minecraft's service. Each launch reads the same account again.
  If it expires, open Prism and launch Minecraft online, then retry. No Microsoft refresh token
  or other launcher's app ID is imported. The source file is never changed.
- Account import supports Prism's `accounts.json`, including a custom path through
  `horizon-launcher account import PATH`. Official-launcher private account storage is not supported.
- The Rust client already has encrypted online-server login. A real online-server join has not
  been verified for this fork. Full Java multiplayer behavior is not claimed.
- **Fabric and Java mod jars do not run in the Rust engine.** Reading a modpack's data and assets
  is not the same as running its mods. A Java-compatible mod runtime remains unimplemented.
- Updates point to this fork. No fork release binaries have been published yet. Release signing
  still needs this fork's own key before distributing builds.
- Launcher data goes in `minecraft-rust-edition-launcher`; saves go in `minecraft-rust-edition`
  under the OS data folder. Existing Minecraft and Horizon folders are not moved or edited.
- Upstream sponsor buttons and hosting ads are removed. Telemetry has no default server.
  Internal crate names and some upstream features and artwork still need a full rename.

## Build

The launcher is Rust: `cargo run --manifest-path launcher/Cargo.toml`.
The game builds with `cargo build --profile fast -p horizon-client`.
These commands build from source; the launcher cannot download a fork release until one exists.
See [the upstream build and setup notes](README.upstream.md) for resource preparation.

## Remaining work

A complete Java/Fabric compatibility layer, broader server checks (including secure chat and
modded servers), finished branding, independent release signing, and packaged releases remain.
The launcher changes alone do not solve these gaps.
