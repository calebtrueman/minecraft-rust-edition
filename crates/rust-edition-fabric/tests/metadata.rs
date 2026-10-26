use std::fs;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use rust_edition_fabric::{
    DependencyKind, InspectError, InspectionWarning, Limits, inspect_directory, inspect_jar,
    inspect_jar_with_limits, inspect_jars,
};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rust-edition-fabric-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn jar(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn jar(manifest: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(MANIFEST, SimpleFileOptions::default())
        .unwrap();
    writer.write_all(manifest.as_bytes()).unwrap();
    for (path, bytes) in extra {
        writer
            .start_file(*path, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn zip_with_duplicate_name(name: &str, files: &[&[u8]]) -> Vec<u8> {
    let name = name.as_bytes();
    let mut archive = Vec::new();
    let mut directory = Vec::new();
    for contents in files {
        let local_offset = archive.len() as u32;
        let checksum = crc32(contents);
        push_u32(&mut archive, 0x0403_4b50);
        push_u16(&mut archive, 20);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u32(&mut archive, checksum);
        push_u32(&mut archive, contents.len() as u32);
        push_u32(&mut archive, contents.len() as u32);
        push_u16(&mut archive, name.len() as u16);
        push_u16(&mut archive, 0);
        archive.extend_from_slice(name);
        archive.extend_from_slice(contents);

        push_u32(&mut directory, 0x0201_4b50);
        push_u16(&mut directory, 20);
        push_u16(&mut directory, 20);
        push_u16(&mut directory, 0);
        push_u16(&mut directory, 0);
        push_u16(&mut directory, 0);
        push_u16(&mut directory, 0);
        push_u32(&mut directory, checksum);
        push_u32(&mut directory, contents.len() as u32);
        push_u32(&mut directory, contents.len() as u32);
        push_u16(&mut directory, name.len() as u16);
        push_u16(&mut directory, 0);
        push_u16(&mut directory, 0);
        push_u16(&mut directory, 0);
        push_u16(&mut directory, 0);
        push_u32(&mut directory, 0);
        push_u32(&mut directory, local_offset);
        directory.extend_from_slice(name);
    }

    let directory_offset = archive.len() as u32;
    let directory_size = directory.len() as u32;
    archive.extend_from_slice(&directory);
    push_u32(&mut archive, 0x0605_4b50);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, files.len() as u16);
    push_u16(&mut archive, files.len() as u16);
    push_u32(&mut archive, directory_size);
    push_u32(&mut archive, directory_offset);
    push_u16(&mut archive, 0);
    archive
}

fn add_zip64_end_records(mut archive: Vec<u8>) -> Vec<u8> {
    let eocd_position = archive.len() - 22;
    let entries = u16::from_le_bytes(
        archive[eocd_position + 10..eocd_position + 12]
            .try_into()
            .unwrap(),
    );
    let central_size = u32::from_le_bytes(
        archive[eocd_position + 12..eocd_position + 16]
            .try_into()
            .unwrap(),
    );
    let central_offset = u32::from_le_bytes(
        archive[eocd_position + 16..eocd_position + 20]
            .try_into()
            .unwrap(),
    );
    let mut converted = archive[..eocd_position].to_vec();
    let zip64_position = converted.len() as u64;
    push_u32(&mut converted, 0x0606_4b50);
    push_u64(&mut converted, 44);
    push_u16(&mut converted, 45);
    push_u16(&mut converted, 45);
    push_u32(&mut converted, 0);
    push_u32(&mut converted, 0);
    push_u64(&mut converted, u64::from(entries));
    push_u64(&mut converted, u64::from(entries));
    push_u64(&mut converted, u64::from(central_size));
    push_u64(&mut converted, u64::from(central_offset));
    push_u32(&mut converted, 0x0706_4b50);
    push_u32(&mut converted, 0);
    push_u64(&mut converted, zip64_position);
    push_u32(&mut converted, 1);

    archive[eocd_position + 8..eocd_position + 12].fill(0xff);
    archive[eocd_position + 12..eocd_position + 20].fill(0xff);
    converted.extend_from_slice(&archive[eocd_position..]);
    converted
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
        }
    }
    !crc
}

const MANIFEST: &str = "fabric.mod.json";

#[test]
fn reads_mod_fields_and_follows_nested_jars_once_by_content() {
    let library = jar(
        r#"{"schemaVersion":1,"id":"sample-lib","version":"2.0.0"}"#,
        &[],
    );
    let first = jar(
        r#"{
          "schemaVersion": 1,
          "id": "first",
          "name": "First Mod",
          "version": "1.0.0",
          "environment": "client",
          "provides": ["first-api"],
          "depends": {"sample-lib": ">=2.0"},
          "entrypoints": {
            "client": [{"value": "example.ClientInit", "adapter": "default", "environment": "client"}]
          },
          "mixins": ["first.mixins.json"],
          "accessWidener": "first.accesswidener",
          "jars": [{"file": "META-INF/jars/sample-lib.jar"}]
        }"#,
        &[
            ("first.mixins.json", b"{}"),
            ("first.accesswidener", b"accessWidener v2 named"),
            ("META-INF/jars/sample-lib.jar", &library),
        ],
    );
    let second = jar(
        r#"{"schemaVersion":1,"id":"second","version":"1.0.0","jars":[{"file":"META-INF/jars/lib.jar"}]}"#,
        &[("META-INF/jars/lib.jar", &library)],
    );
    let dir = TempDir::new();
    let first_path = dir.jar("first.jar", &first);
    let second_path = dir.jar("second.jar", &second);

    let report = inspect_jars([&first_path, &second_path], Limits::default()).unwrap();
    assert_eq!(report.mods.len(), 3);
    let first_mod = report
        .mods
        .iter()
        .find(|module| module.id == "first")
        .unwrap();
    assert_eq!(first_mod.name.as_deref(), Some("First Mod"));
    assert_eq!(first_mod.environment.as_deref(), Some("client"));
    assert_eq!(first_mod.provides, ["first-api"]);
    assert_eq!(
        first_mod.entrypoints["client"][0].value.as_deref(),
        Some("example.ClientInit")
    );
    assert_eq!(
        first_mod.entrypoints["client"][0].environment.as_deref(),
        Some("client")
    );
    assert_eq!(
        first_mod.mixins[0].file.as_deref(),
        Some("first.mixins.json")
    );
    assert_eq!(
        first_mod.access_widener.as_deref(),
        Some("first.accesswidener")
    );
    assert_eq!(first_mod.nested_jars, ["META-INF/jars/sample-lib.jar"]);

    let library_mod = report
        .mods
        .iter()
        .find(|module| module.id == "sample-lib")
        .unwrap();
    assert_eq!(library_mod.sources.len(), 2);
    let dependency = report
        .dependency_facts
        .iter()
        .find(|fact| fact.from_mod == "first" && fact.dependency_id == "sample-lib")
        .unwrap();
    assert_eq!(dependency.kind, DependencyKind::Depends);
    assert_eq!(dependency.candidates_in_scan, ["2.0.0"]);
    assert!(report.mod_id_conflicts.is_empty());
}

#[test]
fn preserves_fabric_accepted_raw_line_breaks_inside_strings() {
    let manifest = "{\n  \"schemaVersion\": 1,\n  \"id\": \"linebreak\",\n  \"version\": \"1\",\n  \"description\": \"first line\nsecond line\"\n}";
    let dir = TempDir::new();
    let path = dir.jar("linebreak.jar", &jar(manifest, &[]));

    let report = inspect_jar(path).unwrap();
    let module = &report.mods[0];
    assert_eq!(
        module.unhandled_metadata["description"].as_str(),
        Some("first line\nsecond line")
    );
    assert!(report.warnings.iter().any(|warning| matches!(
        warning,
        InspectionWarning::RawControlCharactersInString { count: 1, .. }
    )));
}

#[test]
fn rejects_structural_json_errors_and_unsafe_or_missing_nested_paths() {
    let dir = TempDir::new();
    let bad_json = dir.jar(
        "bad-json.jar",
        &jar(
            r#"{"schemaVersion":1,"id":"bad","version":"1", /* comment */}"#,
            &[],
        ),
    );
    assert!(matches!(
        inspect_jar(bad_json),
        Err(InspectError::InvalidMetadata { .. })
    ));

    let unsafe_path = dir.jar(
        "unsafe.jar",
        &jar(
            r#"{"schemaVersion":1,"id":"unsafe","version":"1","jars":[{"file":"../outside.jar"}]}"#,
            &[],
        ),
    );
    assert!(
        matches!(inspect_jar(unsafe_path), Err(InspectError::UnsafeArchivePath { field, .. }) if field == "jars")
    );

    let missing = dir.jar(
        "missing.jar",
        &jar(
            r#"{"schemaVersion":1,"id":"missing","version":"1","jars":[{"file":"META-INF/jars/nope.jar"}]}"#,
            &[],
        ),
    );
    assert!(matches!(
        inspect_jar(missing),
        Err(InspectError::MissingArchiveEntry { .. })
    ));
}

#[test]
fn duplicate_top_level_ids_fail_but_nested_version_choices_are_reported() {
    let dir = TempDir::new();
    let top_a = dir.jar(
        "top-a.jar",
        &jar(r#"{"schemaVersion":1,"id":"same","version":"1"}"#, &[]),
    );
    let top_b = dir.jar(
        "top-b.jar",
        &jar(r#"{"schemaVersion":1,"id":"same","version":"2"}"#, &[]),
    );
    assert!(matches!(
        inspect_jars([&top_a, &top_b], Limits::default()),
        Err(InspectError::DuplicateTopLevelModId { .. })
    ));

    let lib_one = jar(
        r#"{"schemaVersion":1,"id":"shared-lib","version":"1"}"#,
        &[],
    );
    let lib_two = jar(
        r#"{"schemaVersion":1,"id":"shared-lib","version":"2"}"#,
        &[],
    );
    let root_one = dir.jar(
        "root-one.jar",
        &jar(
            r#"{"schemaVersion":1,"id":"root-one","version":"1","jars":[{"file":"META-INF/jars/lib.jar"}]}"#,
            &[("META-INF/jars/lib.jar", &lib_one)],
        ),
    );
    let root_two = dir.jar(
        "root-two.jar",
        &jar(
            r#"{"schemaVersion":1,"id":"root-two","version":"1","jars":[{"file":"META-INF/jars/lib.jar"}]}"#,
            &[("META-INF/jars/lib.jar", &lib_two)],
        ),
    );
    let report = inspect_jars([root_one, root_two], Limits::default()).unwrap();
    assert_eq!(
        report
            .mods
            .iter()
            .filter(|module| module.id == "shared-lib")
            .count(),
        2
    );
    assert_eq!(report.mod_id_conflicts.len(), 1);
    assert_eq!(report.mod_id_conflicts[0].id, "shared-lib");
    assert_eq!(report.mod_id_conflicts[0].alternatives.len(), 2);
}

#[test]
fn nested_depth_limit_is_enforced_and_directory_keeps_other_results() {
    let leaf = jar(r#"{"schemaVersion":1,"id":"leaf","version":"1"}"#, &[]);
    let middle = jar(
        r#"{"schemaVersion":1,"id":"middle","version":"1","jars":[{"file":"leaf.jar"}]}"#,
        &[("leaf.jar", &leaf)],
    );
    let root = jar(
        r#"{"schemaVersion":1,"id":"root","version":"1","jars":[{"file":"middle.jar"}]}"#,
        &[("middle.jar", &middle)],
    );
    let dir = TempDir::new();
    let root_path = dir.jar("root.jar", &root);
    dir.jar(
        "simple.jar",
        &jar(r#"{"schemaVersion":1,"id":"simple","version":"1"}"#, &[]),
    );
    let limits = Limits {
        max_nesting_depth: 0,
        ..Limits::default()
    };
    assert!(matches!(
        inspect_jar_with_limits(&root_path, limits),
        Err(InspectError::LimitExceeded {
            limit: "nested JAR depth",
            ..
        })
    ));

    let invalid = dir.jar(
        "invalid.jar",
        &jar(
            r#"{"schemaVersion":1,"id":"broken","version":"1","jars":[{"file":"missing.jar"}]}"#,
            &[],
        ),
    );
    let scan = inspect_directory(dir.0.clone()).unwrap();
    assert!(
        scan.inspection
            .mods
            .iter()
            .any(|module| module.id == "simple")
    );
    assert!(scan.failures.iter().any(|failure| failure.path == invalid));
    assert!(
        scan.inspection
            .mods
            .iter()
            .any(|module| module.id == "root")
    );
    assert_eq!(scan.failures.len(), 1);
}

#[test]
fn directory_scan_stops_when_roots_exceed_the_shared_nested_jar_budget() {
    let library = jar(
        r#"{"schemaVersion":1,"id":"shared-library","version":"1"}"#,
        &[],
    );
    let dir = TempDir::new();
    for (file, id) in [
        ("a-first.jar", "first"),
        ("b-second.jar", "second"),
        ("c-third.jar", "third"),
    ] {
        let root = jar(
            &format!(
                r#"{{"schemaVersion":1,"id":"{id}","version":"1","jars":[{{"file":"META-INF/jars/library.jar"}}]}}"#
            ),
            &[("META-INF/jars/library.jar", &library)],
        );
        dir.jar(file, &root);
    }
    let limits = Limits {
        max_total_nested_jar_bytes: library.len() as u64,
        ..Limits::default()
    };

    let scan = rust_edition_fabric::inspect_directory_with_limits(&dir.0, limits).unwrap();

    assert!(
        scan.inspection
            .mods
            .iter()
            .any(|module| module.id == "first")
    );
    assert!(
        scan.inspection
            .mods
            .iter()
            .any(|module| module.id == "shared-library")
    );
    assert!(
        !scan
            .inspection
            .mods
            .iter()
            .any(|module| module.id == "second")
    );
    assert!(
        !scan
            .inspection
            .mods
            .iter()
            .any(|module| module.id == "third")
    );
    assert_eq!(scan.inspection.nested_jar_bytes_read, library.len() as u64);
    assert_eq!(scan.failures.len(), 1);
    assert_eq!(scan.failures[0].path.file_name().unwrap(), "b-second.jar");
    let exhaustion = scan.budget_exhausted.unwrap();
    assert_eq!(exhaustion.limit, "total nested JAR bytes");
    assert_eq!(exhaustion.maximum, library.len() as u64);
    assert_eq!(exhaustion.requested, (library.len() * 2) as u64);
}

#[test]
fn failed_root_manifest_still_uses_the_shared_manifest_budget() {
    let invalid_manifest = r#"{"schemaVersion":1,"id":"invalid","version":"1",}"#;
    let valid_manifest = r#"{"schemaVersion":1,"id":"valid","version":"1"}"#;
    let dir = TempDir::new();
    dir.jar("a-invalid.jar", &jar(invalid_manifest, &[]));
    dir.jar("b-valid.jar", &jar(valid_manifest, &[]));
    let limits = Limits {
        max_total_manifest_bytes: invalid_manifest.len() as u64,
        ..Limits::default()
    };

    let scan = rust_edition_fabric::inspect_directory_with_limits(&dir.0, limits).unwrap();

    assert!(scan.inspection.mods.is_empty());
    assert_eq!(
        scan.inspection.manifest_bytes_read,
        invalid_manifest.len() as u64
    );
    assert_eq!(scan.failures.len(), 2);
    assert_eq!(scan.failures[0].path.file_name().unwrap(), "a-invalid.jar");
    assert_eq!(scan.failures[1].path.file_name().unwrap(), "b-valid.jar");
    let exhaustion = scan.budget_exhausted.unwrap();
    assert_eq!(exhaustion.limit, "total manifest bytes");
    assert_eq!(exhaustion.maximum, invalid_manifest.len() as u64);
    assert_eq!(
        exhaustion.requested,
        (invalid_manifest.len() + valid_manifest.len()) as u64
    );
}

#[test]
fn duplicate_zip_names_are_detected_before_the_zip_reader_drops_them() {
    let first = br#"{"schemaVersion":1,"id":"first","version":"1"}"#;
    let second = br#"{"schemaVersion":1,"id":"second","version":"1"}"#;
    let dir = TempDir::new();
    for (name, bytes) in [
        (
            "duplicate-manifest.jar",
            zip_with_duplicate_name(MANIFEST, &[first, second]),
        ),
        (
            "duplicate-manifest-zip64.jar",
            add_zip64_end_records(zip_with_duplicate_name(MANIFEST, &[first, second])),
        ),
    ] {
        let path = dir.jar(name, &bytes);
        assert!(matches!(
            inspect_jar(path),
            Err(InspectError::DuplicateArchiveEntry { path, .. }) if path == MANIFEST
        ));
    }
}

#[test]
fn directory_rejects_more_top_level_jars_than_its_archive_limit() {
    let dir = TempDir::new();
    let bytes = jar(r#"{"schemaVersion":1,"id":"one","version":"1"}"#, &[]);
    dir.jar("one.jar", &bytes);
    dir.jar("two.jar", &bytes);
    let limits = Limits {
        max_archives: 1,
        ..Limits::default()
    };

    assert!(matches!(
        rust_edition_fabric::inspect_directory_with_limits(&dir.0, limits),
        Err(InspectError::LimitExceeded {
            limit: "top-level JAR count",
            actual: 2,
            maximum: 1,
            ..
        })
    ));
}
