# Java mod compatibility in Rust

Minecraft: Rust Edition is a Rust game. A Fabric JAR is not a data pack: its metadata names Java entrypoints, and its code can register callbacks or change Minecraft classes with Mixins. The current mod system runs Minecraft: Rust Edition's WebAssembly components. The pack importer can carry over supported data and assets, but it does not run Java code. Its class scan in `crates/horizon-port-pack/src/classes.rs` reads class names and method names to inventory a mod; it does not interpret methods.

The `horizon-jvm` crate is a first runtime building block. A caller supplies class bytes; the crate loads up to 1,024 classes and 128 MiB total. A test can run a public static method that takes and returns Java `int` values. That method can create up to 4,096 objects. Each object can hold up to 4,096 inherited or local `int` fields, with a total heap cap of 262,144 fields. The runtime supports constructors, `getfield`, `putfield`, `invokevirtual`, `invokespecial`, `invokeinterface`, and `invokestatic` for the small set of int-only methods it understands. It follows class parents, dispatches overrides, and can use one clear interface default method. It rejects missing links, cycles, and competing interface defaults. References use internal handles, and null field or method access returns a clear error.

The instruction set also includes integer arithmetic and branches. It checks instruction boundaries and local-variable use, rejects bytecode it does not understand, caps each run at ten million instructions, nested calls at 64 frames, and class hierarchies at 128 levels and 65,536 links. It rejects classes with initialization code and only allocates concrete classes whose instance fields are all `int`. The reader is not a full JVM class verifier. It is not connected to the game or mod loader. It does not load a Fabric mod, call a Fabric entrypoint, support arrays, implement a Minecraft Java API, apply Mixins, or claim compatibility with any mod.

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

This step now proves that Rust can load a bounded set of class files, follow parent constructors and inherited fields, dispatch an override, and make an interface call in a tiny Java-made example. The next major pieces are class verification, arrays, exceptions, and class initialization. Fabric still needs the loader, transformations, and host APIs listed above before any installed mod can run.

## Try the first slice

`horizon-jvm` is an isolated Cargo workspace until it has a stable design and is ready to join the game workspace:

```sh
cargo test --manifest-path crates/horizon-jvm/Cargo.toml
```

Its tests build tiny class-file fixtures in Rust and check parsing, integer arithmetic, branches, stack errors, unsupported instructions, malformed branch targets, class and object limits, hierarchy cycles and depth, default-method conflicts, and the call-depth limit. Optional tests use `javac` to build small original Java classes, compare static, object, superclass, override, and interface-call results with Java, and check missing links and null-reference errors when `JAVA_HOME` points to a working JDK. Temporary `.class` files are removed after each test. This is not a Fabric integration test.
