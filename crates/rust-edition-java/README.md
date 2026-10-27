# rust-edition-java

This standalone Rust crate wraps the patched Ristretto VM 0.34. It starts an interpreted Java 25
VM from a Ristretto library home, loads classes from paths supplied by the caller, and connects
declared Java `native` methods to registered Rust functions.

The API is small: build a `JavaVm` with the Java home, class paths, and `NativeMethod::sync`
bindings, then call `JavaVm::invoke` with a class name and full method signature. A missing Rust
binding for a Java native method returns `UnsatisfiedLinkError`. A binding for an ordinary Java
method does not replace its bytecode.

The library tests prove that Java class code can call a Rust function through Ristretto. A
separate opt-in test starts the real Fabric Loader and Mixin, runs a tiny original mod's
initializer, and checks that it calls Rust exactly once. The optional
[`rust-edition-fabric-host`](../rust-edition-fabric-host/README.md) uses this adapter to
receive copied state from Minecraft: Rust Edition. This does not make installed mods work
with the native game.
Java tools build the test classes; the Rust adapter runs their code. Generated sources,
classes and JARs stay in temporary folders.

Callers can also supply a `MethodExecutionPolicy` that denies class-name prefixes.
The VM checks the method's owner and receiver before running it, including calls made by
other Java methods. It refuses blocked class startup before marking that class as starting.
Without a policy, the existing behavior stays unchanged. This is a method-call limit,
not a file, network, or memory sandbox.

Ristretto's source and license are kept in `third_party/ristretto_vm` and
`third_party/ristretto_classloader`. The local classloader copy fixes file URLs for paths with
spaces and reserved URL characters. Its upstream version and change are recorded in
`third_party/ristretto_classloader/UPSTREAM.md`.

Prepare the small library home from an installed Java 25 JDK, then run the focused integration
check. The helper uses `java.base.jmod` when the JDK has it; otherwise it extracts `java.base` from
`lib/modules` and rebuilds one JMOD. All generated files stay under ignored `.local/`:

```sh
export JAVA_HOME="/path/to/java-25-jdk"
python3 tools/prepare_ristretto_java_home.py --java-home "$JAVA_HOME"
export RUST_EDITION_JAVA25_HOME="$PWD/.local/ristretto-evaluation/java25-modules-home"
export CARGO_HOME="$PWD/.local/ristretto-evaluation/cargo-home"
export CARGO_TARGET_DIR="$PWD/.local/ristretto-evaluation/target"
cargo test --release --manifest-path crates/rust-edition-java/Cargo.toml
```

The ordinary tests skip their Java fixtures only when both Java-home variables are unset.
If either is set but missing or broken, the tests fail. The prepared home contains `release`,
`jmods/java.base.jmod`, and `lib/tzdb.dat`. That time-zone file lets Fabric write timestamped logs.
The rest of the home is still too small for full Fabric mod loading. Use `--force` to rebuild the
generated folder. No Java library files or class files belong in the repository.

Ristretto 0.34 prefers a JDK's `lib/modules` image when it is present alongside `jmods`. In this
adapter setup that path fails to load `java.lang.ThreadGroup`, so the test uses the stripped scratch
home made by the helper.

## Opt-in Fabric startup check

This check uses Fabric Loader 0.19.5 and Minecraft 26.3. It was tested with ASM 9.10.1 and
Fabric's Mixin 0.17.4+mixin.0.8.7. It checks loader and game versions before starting.
It copies supplied JARs into a temporary folder without changing their contents, keeps
Mixin enabled, and runs only the original test mod. It does not start Minecraft gameplay.

Prepare a separate home with the required JDK modules and security configuration:

```sh
python3 tools/prepare_ristretto_java_home.py --java-home "$JAVA_HOME" \
  --modules java.base,java.compiler,java.datatransfer,java.desktop,java.logging,java.sql,java.transaction.xa,java.xml \
  --output "$PWD/.local/ristretto-evaluation/java25-fabric-home"
export RUST_EDITION_JAVA25_HOME="$PWD/.local/ristretto-evaluation/java25-fabric-home"
```

Point the test at libraries from your own installation. These example paths follow the
usual Minecraft library layout; set the client JAR path separately:

```sh
export MC_LIBRARIES="/path/to/minecraft/libraries"
export RUST_EDITION_JAVA_FABRIC_LOADER_JAR="$MC_LIBRARIES/net/fabricmc/fabric-loader/0.19.5/fabric-loader-0.19.5.jar"
export RUST_EDITION_JAVA_FABRIC_GAME_JAR="/path/to/minecraft-26.3-client.jar"
export RUST_EDITION_JAVA_FABRIC_ASM_JAR="$MC_LIBRARIES/org/ow2/asm/asm/9.10.1/asm-9.10.1.jar"
export RUST_EDITION_JAVA_FABRIC_ASM_ANALYSIS_JAR="$MC_LIBRARIES/org/ow2/asm/asm-analysis/9.10.1/asm-analysis-9.10.1.jar"
export RUST_EDITION_JAVA_FABRIC_ASM_COMMONS_JAR="$MC_LIBRARIES/org/ow2/asm/asm-commons/9.10.1/asm-commons-9.10.1.jar"
export RUST_EDITION_JAVA_FABRIC_ASM_TREE_JAR="$MC_LIBRARIES/org/ow2/asm/asm-tree/9.10.1/asm-tree-9.10.1.jar"
export RUST_EDITION_JAVA_FABRIC_ASM_UTIL_JAR="$MC_LIBRARIES/org/ow2/asm/asm-util/9.10.1/asm-util-9.10.1.jar"
export RUST_EDITION_JAVA_FABRIC_MIXIN_JAR="$MC_LIBRARIES/net/fabricmc/sponge-mixin/0.17.4+mixin.0.8.7/sponge-mixin-0.17.4+mixin.0.8.7.jar"
RUST_EDITION_JAVA_FABRIC_BOOT=1 cargo test --release \
  --manifest-path crates/rust-edition-java/Cargo.toml --test fabric_boot -- --nocapture
```

Without `RUST_EDITION_JAVA_FABRIC_BOOT`, this test skips. Once requested, missing files,
wrong versions, startup failures, or a missing Rust callback fail the test. The ordinary
CI checks do not supply these private libraries and do not prove Fabric startup.

For local formatting and lint checks:

```sh
cargo fmt --manifest-path crates/rust-edition-java/Cargo.toml --check
CARGO_HOME="$PWD/.local/ristretto-evaluation/cargo-home" \
CARGO_TARGET_DIR="$PWD/.local/ristretto-evaluation/target" \
cargo clippy --release --manifest-path crates/rust-edition-java/Cargo.toml --all-targets -- -D warnings
```
