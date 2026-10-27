# Ristretto VM source and local changes

This folder contains `ristretto_vm` 0.34.0, copied from the crates.io source package. The crate
source has SHA-256 `169fe952ae44936e461e52fda3933060e00fc13dafdaf20e6c4e10604093cc3c` and records
upstream Git commit `8448588ecfcedf79a59d697939493f289c1c6ad7`.

- Upstream project: <https://github.com/theseus-rs/ristretto>
- Crates.io version: <https://crates.io/crates/ristretto_vm/0.34.0>
- License: `Apache-2.0 OR MIT`, as declared by the upstream package. The original license texts are
  kept in `LICENSE-APACHE` and `LICENSE-MIT`.

## Local change

The upstream builder did not let a Rust application provide native Java methods. This copy adds
`ConfigurationBuilder::register_native_method(signature, method)`. A host registers each callback
before creating the VM. The signature is the Java class name, method name, and descriptor, such as
`example/Bridge.add(II)I`. The callback runs only when that Java method is marked `native`; it can
override Ristretto's built-in implementation for the same signature. Existing built-ins still serve
other methods.

The callback map is carried into the VM's method registry and used by method resolution, including
the cached virtual and interface call paths. See the changes in `src/configuration/`,
`src/intrinsic_methods/registry.rs`, `src/vm.rs`, `src/thread.rs`, `src/instruction/`, and
`src/method_ref_cache.rs`.

This adds a way for Rust code to answer selected Java native calls. It does not add JNI library
loading, Fabric support, Minecraft classes, or bindings from Java mods to the Rust game and renderer.

This copy also reads the `java.base` descriptor from `jmods/java.base.jmod` when a small Java home
has no `lib/modules` image. The descriptor gives the VM the package map it needs to load internal
base-module resources. In that case, a VM-side `Reflection.getCallerClass` wrapper keeps the normal
Ristretto code for Java homes with an image and skips the Java system-module lookup only when the
JMOD package map is present. The fallback currently maps `java.base` only; the VM logs that other
JMOD modules are not mapped yet.

On Unix hosts, the VM also wraps Java 21 and newer `UnixNativeDispatcher.access0(JI)I`. In this
version, the upstream Ristretto intrinsic returns libc's raw `-1` when a path is missing, but
Java expects the OS error number. The wrapper keeps the upstream path and permission checks and
changes only that `-1` result to the saved OS error. This lets Java report a missing parent
correctly while it builds nested directories. Registered application callbacks still take
precedence over this wrapper.

For Java versions newer than 11, the VM also wraps `Unsafe.compareAndSetReference`. Upstream masks static-field
offsets but then treats the `Class` mirror as an ordinary object, so the CAS can report success
without changing the static field. The local wrapper routes masked offsets to an atomic static
reference update that compares object identity and writes under the same lock. Instance and array
cases still use the upstream path. There is no mirror-to-class reverse lookup yet, so the wrapper
resolves the owner through the active caller's loader and checks the exact mirror before writing;
it fails closed if that lookup returns a same-named class from another loader.

The local patch also adds optional caller-owned state to VM configuration. Native callbacks
can read it through their thread's VM. The adapter clears this state when it is dropped;
a callback that already holds its own `Arc` can finish safely. This does not stop Ristretto's
Java threads or promise that the VM itself has shut down.

An optional `MethodExecutionPolicy` now lets a caller deny class-name prefixes. The thread
checks both the declared method owner and the actual receiver before method execution.
With a policy enabled, nested interpreted calls use that same check instead of the direct
frame-push path. Blocked class initializers are rejected before class startup begins, so
the caller receives `UnsupportedOperationException` without marking the class as broken.
The default configuration has no policy. This limits method calls; it does not restrict
files, network access, reflection, or direct field access as a complete sandbox would.

## Included compiled helper files

These four upstream Java 8 compiler bridge helpers are embedded by `src/compiler/bridge.rs`; the
upstream `src/compiler/README.md` explains how to regenerate them. Their hashes are recorded here
for comparison with upstream. The public-tree check permits only these exact helper paths.
That README's command was adjusted to use this repository's `third_party/ristretto_vm/` path.

| Path under this folder | SHA-256 |
|---|---|
| `src/compiler/classes/org/ristretto/compiler/CompilerBridge.class` | `d7ad0eb41c9a19425784d763f40764e5f2d1d08ffc430c0ca5e3279160a7a627` |
| `src/compiler/classes/org/ristretto/compiler/CompilerBridge$Source.class` | `ad3bbb3ea120a9f1b0587b7c3dfa3b3030d3238f1ddd9c2b55a022f531988dc8` |
| `src/compiler/classes/org/ristretto/compiler/CompilerBridge$MemoryOutput.class` | `668bce5a4a79c20722ed1f2c10d922376e70f743ee0c130b796a2749b0a956e7` |
| `src/compiler/classes/org/ristretto/compiler/CompilerBridge$MemoryFileManager.class` | `6f5d6ca000665400fcaba0f56add52ef0ed3fac198667f9b989df71f343a087f` |

No JDK, Minecraft, Fabric, or other game's compiled classes are included.
