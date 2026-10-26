# Third-party code

## Ristretto JVM

Minecraft: Rust Edition vendors the `ristretto_vm` 0.34.0 Rust Java Virtual Machine in
[`third_party/ristretto_vm`](third_party/ristretto_vm). It comes from the
[Ristretto project](https://github.com/theseus-rs/ristretto) and is licensed `Apache-2.0 OR MIT`.
Both license texts are included beside the source.

The source package hash, upstream commit, local change, and hashes of its four embedded compiler
helpers are listed in [`third_party/ristretto_vm/UPSTREAM.md`](third_party/ristretto_vm/UPSTREAM.md).
The local change lets the Rust host register implementations for selected Java native methods.
It does not make Fabric mods work with the Rust game or renderer by itself.

The crate depends on other Rust packages. Each consuming Cargo workspace records its resolved
versions in its own `Cargo.lock`.
