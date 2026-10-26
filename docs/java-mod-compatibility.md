# Java mod compatibility in Rust

Minecraft: Rust Edition is a Rust game. A Fabric JAR is not a data pack: its metadata names Java entrypoints, and its code can register callbacks or change Minecraft classes with Mixins. The current mod system runs Minecraft: Rust Edition's WebAssembly components. The pack importer can carry over supported data and assets, but it does not run Java code. Its class scan in `crates/horizon-port-pack/src/classes.rs` reads class names and method names to inventory a mod; it does not interpret methods.

The new `horizon-jvm` crate is a first runtime building block. It can load up to 1,024 class files (128 MiB total) when a caller supplies their bytes, then run static methods with Java `int` arguments and results. Its small instruction set includes integer arithmetic, branches, and `invokestatic` calls between loaded classes. It checks instruction boundaries and local-variable use, rejects bytecode it does not understand, caps each run at ten million instructions, and caps nested calls at 64 frames. It rejects methods with Java exception handlers and classes with initialization code or a custom superclass. The reader is not a full JVM class verifier. It is not connected to the game or mod loader. It does not load a Fabric mod, call a Fabric entrypoint, create Java objects, dispatch virtual methods, implement a Minecraft Java API, apply Mixins, or claim compatibility with any mod.

## What a full runtime needs

The [JVM specification](https://docs.oracle.com/javase/specs/jvms/se25/html/) defines far more than a bytecode loop: class verification and linking, object and array behavior, method dispatch, exceptions, threads, and a large Java library. A Fabric mod also expects loader behavior. Fabric's [loader guide](https://docs.fabricmc.net/develop/loader/) describes entrypoints, dependencies, nested JARs and Mixins; its [`fabric.mod.json` guide](https://docs.fabricmc.net/develop/loader/fabric-mod-json) describes entrypoint, environment, and access-widener metadata.

A practical implementation would need these layers, with compatibility measured at each one:

1. Read modern class files, method descriptors, fields, annotations, stack maps and bytecode safely.
2. Verify classes, resolve inheritance and interfaces, create objects and arrays, and implement method dispatch and Java exceptions.
3. Provide enough Java library behavior for real mod dependencies, including lambdas and string concatenation through `invokedynamic`, resources, threads and reflection where needed.
4. Read Fabric metadata, resolve versions and dependencies, find nested JARs and run entrypoints in the correct client or server order.
5. Provide the Minecraft and Fabric APIs that mod code calls, translating their behavior to Minecraft: Rust Edition's game APIs.
6. Transform classes in the right order for Mixins, access wideners, mapping names and other declared transformations.
7. Add isolation, resource limits and failure handling. A Java mod is executable code, so parsing safely is not enough to make running it safe.

This step now proves that Rust can load a bounded set of class files and follow static calls between them. The next major class-runtime pieces are object creation, constructors, instance fields and methods, interface dispatch, and class initialization. Fabric still needs the loader, transformations, and host APIs listed above before any installed mod can run.

## Try the first slice

`horizon-jvm` is an isolated Cargo workspace until it has a stable design and is ready to join the game workspace:

```sh
cargo test --manifest-path crates/horizon-jvm/Cargo.toml
```

Its tests build tiny class-file fixtures in Rust and check parsing, integer arithmetic, branches, stack errors, unsupported instructions, malformed branch targets, missing classes and methods, and the call-depth limit. An optional test uses `javac` to build two small original Java classes, checks cross-class calls against Java results, and checks missing-class and missing-method errors when `JAVA_HOME` points to a working JDK. Temporary `.class` files are removed after the test. This is not a Fabric integration test.
