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

Callers can also supply a `MethodExecutionPolicy` to protect selected Java classes. A policy
blocks calls into those classes and direct reads or writes of their fields. A host can allow one
exact method on one object, and allow exact field reads from that same object. Those rules use
object identity, so another object of the same Java class stays blocked. Inherited methods and
fields can be allowed only when their owner is in that object's loaded class hierarchy. The
policy also keeps each registered object alive for as long as the policy is in use.

`JavaVm::new_with_host_hooks` can replace exact Java method bodies and class initializers with Rust
callbacks. A method callback runs instead of that Java method's body. A class initializer callback
runs after the superclass and constant fields are ready, replaces that class's Java initializer,
and keeps the VM's normal once-only and failure rules. Ordinary native method bindings remain
separate.

For a small host-owned mirror, `allocate_object_without_initialization` loads and links the class,
then creates a zero-filled object without running its class initializer or constructor. The host
can fill private or final fields with the async `set_host_object_field` call. It checks that the
destination belongs to this VM and that the value matches the field type and class hierarchy.
It rejects reference values when it cannot prove which VM's class created them.
`allow_method_on_host_object` and `allow_field_read_on_host_object` then grant the exact getters
needed for that object. Method keys use `owner.method(descriptor)`; field keys use
`owner.field:descriptor`, with JVM names and descriptors. Java bytecode still cannot write a
protected field.

Ristretto currently keeps every Java object it creates alive until the VM closes. That includes
exceptions and their identity rules, even after Java code stops using them. Repeated invalid input
can therefore use more memory during a long session. These focused checks do not prove that
long-running mod gameplay is safe.

Ordinary Java actions still follow class initialization rules. For example, `new`, static field
reads or writes, and static method calls can initialize a class. Method lookup alone does not.
The policy checks Java method calls and direct field bytecode on every access, including cached
calls and field references. When a policy is set, Ristretto uses the interpreter so compiled code
cannot skip those checks. The policy does not restrict reflection or `Unsafe`; it is not a full
Java sandbox.

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
  --modules java.base,java.compiler,java.datatransfer,java.desktop,java.logging,java.prefs,java.sql,java.transaction.xa,java.xml,jdk.zipfs \
  --output "$PWD/.local/ristretto-evaluation/java25-fabric-home"
export RUST_EDITION_JAVA25_HOME="$PWD/.local/ristretto-evaluation/java25-fabric-home"
```

Include `jdk.zipfs` and `java.prefs` for the Fabric startup check. Fabric uses its JAR
file system provider while it scans mods, and `java.desktop` requires `java.prefs`.

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
export RUST_EDITION_JAVA_FABRIC_GUAVA_JAR="$MC_LIBRARIES/com/google/guava/guava/33.6.0-jre/guava-33.6.0-jre.jar"
export RUST_EDITION_JAVA_FABRIC_FAILUREACCESS_JAR="$MC_LIBRARIES/com/google/guava/failureaccess/1.0.3/failureaccess-1.0.3.jar"
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
