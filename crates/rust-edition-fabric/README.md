# Fabric metadata reader

This crate reads fabric.mod.json files from Fabric mod JARs. It reads the file names it needs,
checks declared mixin and access-widener paths, and follows nested JARs listed by the mod. It never
loads or runs Java classes and never changes the JARs.

    let scan = rust_edition_fabric::inspect_directory("/path/to/mods")?;
    for module in &scan.inspection.mods {
        println!("{} {}", module.id, module.version);
    }

The reader follows nested JARs with size, count, and depth limits. One directory scan shares its
byte and archive limits across every top-level JAR, including JARs that fail to parse. If a shared
limit stops the scan, `budget_exhausted` explains which one, and the report keeps results from
earlier valid JARs. It rejects unsafe or missing archive paths and duplicate ZIP names. It also
rejects duplicate top-level mod IDs. Identical nested JARs are listed once by SHA-256 with all their
source paths. Different contents with the same mod ID are reported as alternatives in
`mod_id_conflicts`.

Dependency entries are kept as written. The report says whether each dependency ID appeared in the
scanned JARs, but it does not compare versions, choose among alternatives, or decide whether a mod
can run. IDs such as minecraft, java, and fabricloader may come from the runtime and will be shown
as absent from the scanned JARs. Mixins, language adapters, access wideners, and entrypoints are
only reported; none are applied or executed. This crate bundles no Fabric Loader code and does not
make Fabric mods run inside Horizon.

Fabric Loader accepts raw control characters inside quoted metadata strings. The Rust reader
matches that behavior by escaping those characters in a temporary parsing buffer, preserving their
values, and adding a warning to the report. It leaves the original archive untouched. Other
malformed JSON is rejected. Unrecognized metadata fields and unsupported schema versions are
reported so callers can see what this reader did not interpret. The behavior was checked against
Fabric Loader's own [metadata parser](https://github.com/FabricMC/fabric-loader/blob/master/src/main/java/net/fabricmc/loader/impl/metadata/ModMetadataParser.java),
[version 1 metadata parser](https://github.com/FabricMC/fabric-loader/blob/master/src/main/java/net/fabricmc/loader/impl/metadata/V1ModMetadataParser.java),
and [JSON reader](https://github.com/FabricMC/fabric-loader/blob/master/src/main/java/net/fabricmc/loader/impl/lib/gson/JsonReader.java).

inspect_directory checks the JAR files directly inside one folder. It returns successful results
and per-file errors, so one unreadable mod does not hide the others. inspect_jar and inspect_jars
are strict entry points for one JAR or a chosen set of JARs. From the repository root, run
cargo run --manifest-path crates/rust-edition-fabric/Cargo.toml --example inspect -- /path/to/mods
to print a scan summary, errors, and the IDs and versions it found. The example only reads
metadata. It exits with an error code when any JAR fails.
