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

This copy also reads module descriptors from the supplied JMOD files when a small Java home
has no `lib/modules` image. The descriptors give the VM package maps and service providers,
including the ZIP file provider when `jdk.zipfs` is present. In that case, a VM-side
`Reflection.getCallerClass` wrapper keeps the normal
Ristretto code for Java homes with an image and skips the Java system-module lookup only when the
JMOD package map is present.

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

For Java 11 and newer, the VM also handles plain `VarHandle.get` reads from byte-array view handles
for short, char, int, float, long, and double values. It reads the requested byte range using the
handle's byte order and preserves signed integer and floating-point bits. This covers the plain read
used by the current Java 25 class-loading path, including unaligned offsets. It does not add byte
array view writes, volatile or atomic access modes, ByteBuffer views, or special handling for lazy
and indirect VarHandle wrappers; those still use upstream behavior. The Java 25 comparison lives in
`crates/rust-edition-java/tests/byte_array_var_handle.rs`.

The local patch also adds optional caller-owned state to VM configuration. Native callbacks
can read it through their thread's VM. The adapter clears this state when it is dropped;
a callback that already holds its own `Arc` can finish safely. This does not stop Ristretto's
Java threads or promise that the VM itself has shut down.

An optional `MethodExecutionPolicy` lets a caller deny class-name prefixes and grant exact
method calls or direct field reads to selected host objects. Rules compare Java object identity,
not class name or field contents, and keep registered object values alive for the policy's
lifetime. Every interpreter method call and field instruction checks the rules, even after its
reference has been cached. A policy-enabled VM does not create a JIT compiler, so compiled code
cannot skip those checks. Protected Java field writes stay blocked. The host can set mirror fields
through a separate VM call that checks the destination VM, field kind, and reference class
identity; a reference whose VM ownership cannot be checked is rejected. That trusted host write can
set private or final instance fields.

The Java adapter exposes `allocate_object_without_initialization`, async
`set_host_object_field`, `allow_method_on_host_object`, and
`allow_field_read_on_host_object` for host mirrors. The raw mirror allocator loads and links a
class without initializing it, then creates a zero-filled object without calling a constructor.
`same_object` compares values by Java reference identity.
This does not change Java's normal class initialization rules. Ordinary `new`, `getstatic`,
`putstatic`, and `invokestatic` still initialize classes as required. Blocked class initializers
fail before class startup begins, so the caller receives `UnsupportedOperationException` without
marking the class as broken. The default VM configuration has no policy. This is a direct-bytecode
guard, not a sandbox for reflection, `Unsafe`, files, or network access.

A separate host-method registry can replace the body of an exact Java method with Rust code,
including ordinary methods. These bindings do not change the native-only callback registry.
The host class-initializer registry replaces only the named class's startup body, preserving
the VM's once-only startup and failed-startup rules. Exact replacements can run where the
method policy blocks the original Java body; they grant no other methods or fields.
The adapter rejects ordinary-method bindings for `<clinit>` and duplicate native/host keys.

The host can also allow an exact inherited method or field read on one object after checking
that its owner belongs to the object's loaded class hierarchy. These permissions remain tied
to that object, and never allow constructors, class initializers, or field writes.

Method reference lookup now loads and links through the referring class without initializing the
target. The previous call to `class_java_str` ran `<clinit>` too early, even for an
`invokevirtual` call on an existing object. `invokestatic` still performs its explicit
initialization step after resolution. This follows [JVMS 25, section 5.5](https://docs.oracle.com/javase/specs/jvms/se25/html/jvms-5.html#jvms-5.5), which limits initialization to active uses such as
creating an instance, reading or writing a static field, or calling a static method.

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
