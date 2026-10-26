# Ristretto classloader source and local change

This folder contains `ristretto_classloader` 0.34.0 from the crates.io source package. Its package
SHA-256 is `726b54fc3aab8cd127c67015306c66aef2698517165b1f004fbe226bb03d8bc5`. Cargo records upstream
Git commit `8448588ecfcedf79a59d697939493f289c1c6ad7`, in the `ristretto_classloader` directory.

- Upstream project: <https://github.com/theseus-rs/ristretto>
- Crates.io package: <https://crates.io/crates/ristretto_classloader/0.34.0>
- License: `Apache-2.0 OR MIT`, as declared by the upstream package. The license texts are kept in
  `LICENSE-APACHE` and `LICENSE-MIT`.

## Local change

The upstream classloader made file URLs by joining `file:` to a raw path string. Spaces and URL
characters such as `#` and `%` then had the wrong meaning. This copy uses the `url` crate's
`Url::from_file_path` to build an escaped file URL, and keeps the trailing slash for directories.
No Java, Minecraft, or Fabric files are included.
