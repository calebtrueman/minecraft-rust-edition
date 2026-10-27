# Fabric helper test

This helper runs Fabric Loader 0.19.5 through the Rust-based Ristretto VM, using
Minecraft 26.3 libraries. The game can send it copied player and clock values.
A small original test mod returns a hash of those values, plus a call count.

This does not support existing Fabric mods. The test uses a custom
`onSnapshot(String)` method, not Fabric's tick events. Minecraft calls and Mixins
still need to be connected to Rust game behavior. Java calls on Minecraft game
and rendering classes are blocked in this helper until that work exists.

## Build and run the checks

This crate has a separate Cargo workspace, like `rust-edition-java`:

```sh
cargo build --release --manifest-path crates/rust-edition-fabric-host/Cargo.toml
cargo test --release --manifest-path crates/rust-edition-fabric-host/Cargo.toml
```

The Fabric check skips unless `RUST_EDITION_FABRIC_HOST_TEST=1` is set. It also
needs the local Java 25, Fabric, ASM, Mixin, and Minecraft paths listed in
`tests/host_bridge.rs`. The Java library home must contain all eight modules used
by the Fabric startup check, along with its security and time-zone files. See
[the Java adapter](../rust-edition-java/README.md) for preparing those files.
No game or loader files are bundled here.

With those same path variables set and the game built, run the native game check:

```sh
python3 tools/test_java_mod_bridge_game.py \
  --bin-dir /absolute/path/to/game-binaries \
  --helper /absolute/path/to/rust-edition-fabric-host \
  --resources /absolute/path/to/prepared-game-resources
```

The check creates its own mod and fresh game folders. It checks the returned
hash against the exact snapshot from a real game tick, checks the cleared
snapshot after leaving the world, and checks helper cleanup after exit. It runs
both supported local game-loop modes by default. Java development tools compile
the fixture; the player-side helper runs Rust code.

## Optional game settings

Set `HORIZON_MOD_BRIDGE_HELPER` to the helper's absolute executable path and
`HORIZON_MOD_BRIDGE_CONFIG` to an absolute config file path. The test above builds
that strict JSON config from the supplied library paths. Without these settings,
the game does not start the helper. `HORIZON_MOD_BRIDGE_SCRATCH` can choose the
parent folder for its temporary working files.

The game waits for Fabric startup before sending snapshots. Replies have a
256-byte text limit and a one-second deadline. Startup has a 60-second deadline.
The queue holds at most four outstanding snapshots; exceeding it stops the
connection instead of silently dropping updates.

The helper is a separate process with a fresh working folder and a small set of
environment variables. **It is not a file or network sandbox.** It retains the
user's operating-system access. Only use trusted test JARs.
