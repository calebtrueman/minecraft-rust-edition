use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rust_edition_fabric::{DirectoryScanReport, inspect_directory};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let Some(directory) = args.next().map(PathBuf::from) else {
        eprintln!("usage: cargo run --example inspect -- <mods-directory>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("usage: cargo run --example inspect -- <mods-directory>");
        return ExitCode::from(2);
    }

    let scan = match inspect_directory(&directory) {
        Ok(scan) => scan,
        Err(error) => {
            let message = redact_root(&error.to_string(), &directory);
            eprintln!(
                "could not scan {}: {}",
                safe_path(&directory),
                safe_text(&message)
            );
            return ExitCode::from(1);
        }
    };
    print_scan(&scan, &directory);
    if scan.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_scan(scan: &DirectoryScanReport, directory: &Path) {
    let report = &scan.inspection;
    let top_level_mods = report
        .mods
        .iter()
        .filter(|module| !module.top_level_sources.is_empty())
        .count();
    println!("Top-level mods read: {top_level_mods}");
    println!("Unique metadata-bearing JARs: {}", report.mods.len());
    println!("Archive attempts: {}", report.archives_read);
    println!("Nested JAR bytes read: {}", report.nested_jar_bytes_read);
    println!("Manifest bytes read: {}", report.manifest_bytes_read);
    println!(
        "Mod IDs with different-content alternatives: {}",
        report.mod_id_conflicts.len()
    );

    for module in &report.mods {
        println!("{} {}", safe_text(&module.id), safe_text(&module.version));
    }
    for conflict in &report.mod_id_conflicts {
        let versions = conflict
            .alternatives
            .iter()
            .map(|alternative| safe_text(&alternative.version))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "alternative contents: {} ({versions})",
            safe_text(&conflict.id)
        );
    }
    for failure in &scan.failures {
        let name = failure
            .path
            .file_name()
            .unwrap_or(failure.path.as_os_str())
            .to_string_lossy();
        let message = redact_root(&failure.message, directory);
        println!("error: {}: {}", safe_text(&name), safe_text(&message));
    }
    if let Some(exhaustion) = &scan.budget_exhausted {
        let archive = redact_root(&exhaustion.archive, directory);
        println!(
            "scan stopped at the shared {} limit: {} requested, {} allowed ({})",
            exhaustion.limit,
            exhaustion.requested,
            exhaustion.maximum,
            safe_text(&archive)
        );
    }
}

fn safe_path(path: &Path) -> String {
    let display = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    safe_text(&display)
}

fn redact_root(message: &str, directory: &Path) -> String {
    if !directory.is_absolute() {
        return message.to_owned();
    }
    let root = directory.to_string_lossy();
    let trimmed = root.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        message.to_owned()
    } else {
        message
            .replace(root.as_ref(), "<mods>")
            .replace(trimmed, "<mods>")
    }
}

fn safe_text(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => safe.push_str("\\n"),
            '\r' => safe.push_str("\\r"),
            '\t' => safe.push_str("\\t"),
            character if character.is_control() => {
                safe.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => safe.push(character),
        }
    }
    safe
}
