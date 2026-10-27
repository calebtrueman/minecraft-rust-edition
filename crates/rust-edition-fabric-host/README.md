# Fabric helper test

This helper runs Fabric Loader 0.19.5 through the Rust-based Ristretto VM, using
Minecraft 26.3 libraries. It receives copied player, clock, and framebuffer
values. For each completed client step, it calls the real Fabric
`ClientTickEvents.END_CLIENT_TICK` event with persistent Minecraft and Window
mirror objects. The mirror getters return the values from that frame.

This still does not support existing Fabric mods. The probe mod uses a custom
`onSnapshot(String)` test callback and checks a small Fabric event path. It does
not run Minecraft gameplay or connect Java game code to the Rust engine. The
host allocates its two mirror objects without running Minecraft or Window
constructors. It permits only the exact window getter and framebuffer field
reads on those object identities; other Minecraft and rendering calls stay
blocked.

The config names the installed Fabric API root JAR in `fabric_api_jar`. The
host reads its declared nested paths and stages only `fabric-api-base` and
`fabric-lifecycle-events-v1` as mods and class path entries. Those JARs keep
their own metadata. The outer aggregate is not copied into the helper, so this
does not claim support for the rest of Fabric API. This config shape uses schema
version 2.

The helper replaces a small set of vanilla 26.3 `Identifier` factories,
validators, accessors, path text helpers, and equality, hash, ordering, and name
formatting methods with Rust code. It does not run `Identifier.<clinit>` or its
Java method bodies. The computed `CODEC`, `STREAM_CODEC`, and `ERROR_INVALID`
fields are not implemented. Direct Java reads are blocked by the host policy;
the fixture checks that a `CODEC` read fails. Reflection is outside that policy
and may see the fields' default null values. The fixture also compares the
factory null-order cases against exact Java 25 results.
Brigadier readers, codec operations, `resolveAgainst`, and the
`withPath(UnaryOperator)` overload remain blocked. Invalid factories create a
typed `IdentifierException` with its message and cause fields set by Rust.

## Build and run the checks

This crate has a separate Cargo workspace, like `rust-edition-java`:

```sh
cargo build --release --manifest-path crates/rust-edition-fabric-host/Cargo.toml
cargo test --release --manifest-path crates/rust-edition-fabric-host/Cargo.toml
```

The Fabric check skips unless `RUST_EDITION_FABRIC_HOST_TEST=1` is set. It also
needs the local Java 25, Fabric Loader, Fabric API, ASM, Mixin, Guava,
failureaccess, and Minecraft paths listed in `tests/host_bridge.rs`. It reads
Minecraft's pinned 26.3 version manifest, verifies the ordinary Java library
paths and hashes, and skips native-classifier archives. Set
`RUST_EDITION_JAVA_FABRIC_API_JAR` to the original 0.162.0+26.3 aggregate JAR.
The fixture extracts the declared nested JARs for `javac`; the helper extracts
the same two files into its own fresh folder from the `fabric_api_jar` config
path.
The Java library home must contain all ten modules used by the Fabric startup
check, along with its security and time-zone files. See
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
hash against each exact frame, sends real title and world ticks plus synthetic
initial and world-close updates, and checks helper cleanup after exit. It runs
both supported local game-loop modes by default. Java development tools compile
the fixture; the player-side helper runs Rust code.

## Optional game settings

Set `HORIZON_MOD_BRIDGE_HELPER` to the helper's absolute executable path and
`HORIZON_MOD_BRIDGE_CONFIG` to an absolute config file path. The test above builds
that strict JSON config from the supplied library paths. Without these settings,
the game does not start the helper. `HORIZON_MOD_BRIDGE_SCRATCH` can choose the
parent folder for its temporary working files.

The game waits for Fabric startup before sending snapshots. Replies have a
256-byte text limit and a one-second deadline. Startup has a 120-second deadline.
The queue holds at most twelve outstanding frames, allowing ten catch-up ticks
plus two state updates. Exceeding it stops the connection instead of silently
dropping updates.

The helper is a separate process with a fresh working folder and a small set of
environment variables. **It is not a file or network sandbox.** It retains the
user's operating-system access. Only use trusted test JARs.
