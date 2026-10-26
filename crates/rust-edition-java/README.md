# rust-edition-java

This standalone Rust crate wraps the patched Ristretto VM 0.34. It starts an interpreted Java 25
VM from a Ristretto library home, loads classes from paths supplied by the caller, and connects
declared Java `native` methods to registered Rust functions.

The API is small: build a `JavaVm` with the Java home, class paths, and `NativeMethod::sync`
bindings, then call `JavaVm::invoke` with a class name and full method signature. A missing Rust
binding for a Java native method returns `UnsatisfiedLinkError`. A binding for an ordinary Java
method does not replace its bytecode.

This proves that Java class code can call a Rust function through Ristretto. It does not read a
Fabric mod, launch a Java process, load Fabric, connect to Minecraft: Rust Edition, or provide a
Minecraft or Fabric API. The `javac` test source and class files live only in a temporary folder.

Ristretto's source, license, and upstream credit are in `third_party/ristretto_vm`.

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

The test skips its Java fixture only when both environment variables are unset. If either is set
but missing or broken, the test fails. The prepared home contains only `release` and
`jmods/java.base.jmod`; it is enough for this fixture, not a full Java library for Fabric mods.
Use `--force` to rebuild the generated folder. No Java library files or class files belong in the
repository.

Ristretto 0.34 prefers a JDK's `lib/modules` image when it is present alongside `jmods`. In this
adapter setup that path fails to load `java.lang.ThreadGroup`, so the test uses the stripped scratch
home made by the helper.

For local formatting and lint checks:

```sh
cargo fmt --manifest-path crates/rust-edition-java/Cargo.toml --check
CARGO_HOME="$PWD/.local/ristretto-evaluation/cargo-home" \
CARGO_TARGET_DIR="$PWD/.local/ristretto-evaluation/target" \
cargo clippy --release --manifest-path crates/rust-edition-java/Cargo.toml --all-targets -- -D warnings
```
