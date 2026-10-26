# Java mod compatibility in Rust

Minecraft: Rust Edition is a Rust game. A Fabric JAR is not a data pack: its metadata names Java entrypoints, and its code can register callbacks or change Minecraft classes with Mixins. The current mod system runs Minecraft: Rust Edition's WebAssembly components. The pack importer can carry over supported data and assets, but it does not run Java code. Its class scan in `crates/horizon-port-pack/src/classes.rs` reads class names and method names to inventory a mod; it does not interpret methods.

The `horizon-jvm` crate is a first runtime building block. A caller supplies class bytes; the crate loads up to 1,024 classes and 128 MiB total. A test can run a public static method that takes and returns Java `int` values. That method can create up to 4,096 objects. Each object can hold up to 4,096 inherited or local `int` fields, with a total heap cap of 262,144 fields. The runtime supports constructors, `getfield`, `putfield`, `invokevirtual`, `invokespecial`, `invokeinterface`, and `invokestatic` for the small set of int-only methods it understands. It follows class parents, dispatches overrides, and can use one clear interface default method. It rejects missing links, cycles, and competing interface defaults. References use internal handles, and null field or method access returns a clear error.


The instruction set also includes integer arithmetic and branches. It checks instruction boundaries and local-variable use, rejects bytecode it does not understand, caps each run at ten million instructions, nested calls at 64 frames, and class hierarchies at 128 levels and 65,536 links. It rejects classes with initialization code and only allocates concrete classes whose instance fields are all `int`. The reader is not a full JVM class verifier. It is not connected to the game or mod loader. It does not load a Fabric mod, call a Fabric entrypoint, support arrays, implement a Minecraft Java API, apply Mixins, or claim compatibility with any mod.

## Ristretto callback proof

The separate `rust-edition-java` crate wraps patched Ristretto VM 0.34. The caller gives it a Ristretto library home, mod class paths and Rust native-method functions. It runs in interpreted mode. A tiny class built by `javac` calls a Rust `hostCall` function and gets `42`. The test also checks that an unregistered native method fails with `UnsatisfiedLinkError`, and a Rust callback does not replace ordinary Java bytecode. Java sources and class files stay in a temporary test folder.

Ristretto 0.34 prefers a JDK's `lib/modules` image when that image and `jmods` are both present. In this adapter, that path fails to load `java.lang.ThreadGroup`. The helper `tools/prepare_ristretto_java_home.py` makes an ignored scratch home with `release`, `jmods/java.base.jmod`, and the JDK's `lib/tzdb.dat` time-zone file. It copies `java.base.jmod` from a normal JDK, or extracts `java.base` with `jimage` and rebuilds it with `jmod` when the JDK has no JMODs. This small library home is enough for the adapter fixtures. It is not enough to run Fabric mods.

Further Java 25 comparisons check file URLs for class folders and JARs whose paths contain spaces, Unicode, `#` and `%`. A separate check verifies text normalization, the `java.base` module name, and denial of access to a private Java resource. These checks use original test classes. The JMOD module fix currently maps only `java.base`; it does not map every Java module.

Native callbacks can also use caller-owned Rust state. A two-VM test checks that their counters
stay separate and that dropping the adapter releases its state. Dropping the adapter does not
stop Ristretto's Java threads. Another Java comparison checks folder creation with missing
parents, existing files, and linked folders on Unix hosts.

This proves that Ristretto can run these supplied Java classes and call a registered Rust function. The game and loader do not use this crate yet. It does not load a Fabric JAR, call a Fabric entrypoint, provide Minecraft or Fabric classes, apply Mixins, or make any installed mod compatible.

## Next step toward a game bridge

Build a separate, opt-in `horizon-fabric-bridge` for Java mods. Keep it out of the existing `horizon-mods` WebAssembly runtime, whose sandbox is part of its promise. The adapter can run Java code, but it does not yet limit file or network access like WebAssembly does. Do not start untrusted Java JARs in the game process until that safety boundary is in place.

The first useful bridge is one small vertical slice: read `fabric.mod.json` from an installed JAR, add its declared entrypoint and required JARs to `JavaVm::new`, provide the Fabric classes that entrypoint needs, and call `ModInitializer.onInitialize`. Add one supported tick event whose Rust native function registers a Java callback. At `Game::tick_mods` in `crates/horizon-client/src/game/mods.rs`, deliver a copied game snapshot and collect bounded actions, following the snapshot pattern already used by `crates/horizon-mods`. Never hand Java code a mutable pointer to `Game`.

First prove that route with a small Java JAR built in a test: its initializer registers the tick event, and the game test hook confirms the callback ran. Then try one installed mod whose listed classes and calls fit that same supported slice. Use `crates/horizon-port-pack/src/classes.rs` to flag Mixins and other unsupported code early; its scan is an inventory, not a loader. Add more events and Java/Fabric classes only when a fixture tests them. This path gets one selected mod through the native bridge; it does not make arbitrary Fabric mods compatible.

## What a full runtime needs

The [JVM specification](https://docs.oracle.com/javase/specs/jvms/se25/html/) defines far more than a bytecode loop: class verification and linking, object and array behavior, method dispatch, exceptions, threads, and a large Java library. A Fabric mod also expects loader behavior. Fabric's [loader guide](https://docs.fabricmc.net/develop/loader/) describes entrypoints, dependencies, nested JARs and Mixins; its [`fabric.mod.json` guide](https://docs.fabricmc.net/develop/loader/fabric-mod-json) describes entrypoint, environment, and access-widener metadata.

A practical implementation would need these layers, with compatibility measured at each one:

1. Read modern class files, method descriptors, fields, annotations, stack maps and bytecode safely.
2. Verify classes, add arrays and Java exceptions, and cover more of the JVM's inheritance and interface rules. The current hierarchy and dispatch support is a small slice.
3. Provide enough Java library behavior for real mod dependencies, including lambdas and string concatenation through `invokedynamic`, resources, threads and reflection where needed.
4. Read Fabric metadata, resolve versions and dependencies, find nested JARs and run entrypoints in the correct client or server order.
5. Provide the Minecraft and Fabric APIs that mod code calls, translating their behavior to Minecraft: Rust Edition's game APIs.
6. Transform classes in the right order for Mixins, access wideners, mapping names and other declared transformations.
7. Add isolation, resource limits and failure handling. A Java mod is executable code, so parsing safely is not enough to make running it safe.

The `horizon-jvm` prototype proves that Rust can load a bounded set of class files, follow parent constructors and inherited fields, dispatch an override, and make an interface call in a tiny Java-made example. The separate Ristretto callback proof is described above; neither runtime is connected to the game. The next major pieces include class verification, arrays, exceptions and class initialization. Fabric still needs the loader, transformations and host APIs listed above before any installed mod can run.

## Try the first slice

`horizon-jvm` is an isolated Cargo workspace until it has a stable design and is ready to join the game workspace:

```sh
cargo test --manifest-path crates/horizon-jvm/Cargo.toml
```

Its tests build tiny class-file fixtures in Rust and check parsing, integer arithmetic, branches, stack errors, unsupported instructions, malformed branch targets, class and object limits, hierarchy cycles and depth, default-method conflicts, and the call-depth limit. Optional tests use `javac` to build small original Java classes, compare static, object, superclass, override, and interface-call results with Java, and check missing links and null-reference errors when `JAVA_HOME` points to a working JDK. Temporary `.class` files are removed after each test. This is not a Fabric integration test.

To run the Ristretto proof, set `JAVA_HOME` to a Java 25 JDK with `javac`, prepare the small class
library home, and point `RUST_EDITION_JAVA25_HOME` at it:

```sh
export JAVA_HOME="/path/to/java-25-jdk"
python3 tools/prepare_ristretto_java_home.py --java-home "$JAVA_HOME"
export RUST_EDITION_JAVA25_HOME="$PWD/.local/ristretto-evaluation/java25-modules-home"
export CARGO_HOME="$PWD/.local/ristretto-evaluation/cargo-home"
export CARGO_TARGET_DIR="$PWD/.local/ristretto-evaluation/target"
cargo test --release --manifest-path crates/rust-edition-java/Cargo.toml
```

The helper writes `release`, `java.base.jmod`, and `lib/tzdb.dat` under ignored `.local/` scratch space.
The tests compile original Java fixtures into temporary folders and compare their results with Java 25. They are not Fabric or game tests.

## Read installed mod metadata

`crates/rust-edition-fabric` reads `fabric.mod.json` and follows the nested jars it declares.
It reports entrypoints, dependencies, mixins, access wideners and different copies of the same
mod ID. It does not choose versions, apply transforms, or run Java code.

The reader has shared limits for archive count, nested bytes and metadata bytes, including work
spent on failed archives. It checks duplicate ZIP names before the ZIP library can hide them.
A directory scan keeps earlier valid results and reports file errors or a reached limit.

To inspect a mod folder without changing its jars:

```sh
cargo run --manifest-path crates/rust-edition-fabric/Cargo.toml --example inspect -- /path/to/mods
```

See [the reader's notes](../crates/rust-edition-fabric/README.md) for its exact scope and Fabric
format references. The GitHub mod checks run this crate's tests separately from the game.
