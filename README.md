# Minecraft: Rust Edition

An unofficial Rust game and launcher, forked from [Horizon](https://github.com/ludovic111/horizon).
Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.
The original MIT license and credits remain in [LICENSE](LICENSE).
Reused libraries and local changes are listed in [THIRD_PARTY.md](THIRD_PARTY.md).

## Fork status

This is work in progress, not a finished replacement for Java Edition.

- Gameplay stays in Rust. Java is used to prepare the game files from your own installation.
- The launcher reads the existing Minecraft files it supports. Its current game version is pinned
  in `research-lock.json`; an older Java installation alone is not enough.
- **Import launcher session** reads the official launcher's selected Minecraft account, or
  Prism when no official account file exists. Ownership and profile are checked with Minecraft.
  On macOS, if the official file has a blank token, it can read the matching session from your
  running Minecraft game. The imported session can then be reused until it expires; sign in and
  launch through the source launcher again to renew it. No Microsoft refresh token or other
  launcher's app ID is imported. Source files are never changed.
- A custom account file can be selected with `horizon-launcher account import PATH`.
  Running-game session import is currently macOS-only. It does not decrypt launcher credentials.
- The Rust client passed a local test against the unchanged Java 26.3 server with a real
  account: encrypted login, 49 loaded chunks, 25 signed chat messages, `/list` and a signed
  private command to the same player. This does not establish
  full Java multiplayer behavior or modded-server compatibility.
- **Fabric and Java mod jars do not run in the Rust engine.** Reading a modpack's data and assets
  is not the same as running its mods. Separate tests cover a small Rust bytecode interpreter
  and a bridge built on the Rust-based Ristretto VM. Neither is connected to the game yet.
  An isolated test starts Fabric and Mixin through that bridge and runs one tiny test mod.
  See [the compatibility work](docs/java-mod-compatibility.md).
- Updates point to this fork. No fork release binaries have been published yet. Release signing
  still needs this fork's own key before distributing builds.
- Launcher data goes in `minecraft-rust-edition-launcher`; saves go in `minecraft-rust-edition`
  under the OS data folder. Existing Minecraft and Horizon folders are not moved or edited.
- Upstream sponsor buttons and hosting ads are removed. Telemetry has no default server,
  and Discord presence requires an explicitly configured app ID.
  The launcher and packaged app use a new cube icon. Internal crate names and some upstream
  features still need a full rename.

## Build

The launcher is Rust: `cargo run --manifest-path launcher/Cargo.toml`.
The game builds with `cargo build --profile fast -p horizon-client`.
These commands build from source; the launcher cannot download a fork release until one exists.
See [source build instructions](docs/source-builds.md) for packaging and local installation.

On macOS, a local package was installed and prepared from an existing official-launcher
Fabric profile for the pinned game version. The launcher then started a Rust test world
with 49 generated chunks and no reported error. This resource-preparation test did not load
Fabric mods. The separate account-checked multiplayer test is described above.

## Remaining work

A complete Java/Fabric compatibility layer, broader server checks (including long secure-chat
sessions and modded servers), finished branding, independent release signing, and packaged
releases remain.
The launcher changes alone do not solve these gaps.
