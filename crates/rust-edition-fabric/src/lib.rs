//! Read Fabric mod metadata without loading or running Java code.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Seek};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

pub mod version_predicate;

use version_predicate::matching_versions;

const MANIFEST: &str = "fabric.mod.json";

/// Limits for one scan. Defaults allow ordinary large mod sets while bounding archive work.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_root_jar_bytes: u64,
    pub max_nested_jar_bytes: u64,
    pub max_total_nested_jar_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_total_manifest_bytes: u64,
    pub max_archives: usize,
    pub max_archive_entries: usize,
    pub max_nesting_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_root_jar_bytes: 256 * 1024 * 1024,
            max_nested_jar_bytes: 64 * 1024 * 1024,
            max_total_nested_jar_bytes: 256 * 1024 * 1024,
            max_manifest_bytes: 1024 * 1024,
            max_total_manifest_bytes: 16 * 1024 * 1024,
            max_archives: 512,
            max_archive_entries: 100_000,
            max_nesting_depth: 8,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct InspectionReport {
    /// One entry per unique JAR content, including nested JARs.
    pub mods: Vec<ModInfo>,
    pub mod_id_conflicts: Vec<ModIdConflict>,
    pub dependency_facts: Vec<DependencyFact>,
    pub warnings: Vec<InspectionWarning>,
    /// Archive read attempts, including identical nested JAR references.
    pub archives_read: usize,
    pub nested_jar_bytes_read: u64,
    pub manifest_bytes_read: u64,
}

#[derive(Clone, Debug)]
pub struct ModInfo {
    pub id: String,
    pub name: Option<String>,
    pub version: String,
    pub schema_version: Option<u32>,
    pub environment: Option<String>,
    pub provides: Vec<String>,
    pub dependencies: DependencyDeclarations,
    pub entrypoints: BTreeMap<String, Vec<Entrypoint>>,
    pub mixins: Vec<MixinConfig>,
    pub access_widener: Option<String>,
    pub nested_jars: Vec<String>,
    /// The first path seen, followed by paths to identical archive contents.
    pub sources: Vec<String>,
    /// Paths supplied as top-level mods rather than found inside another JAR.
    pub top_level_sources: Vec<String>,
    pub content_sha256: String,
    /// Metadata kept for callers, but not interpreted by this reader.
    pub unhandled_metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default)]
pub struct DependencyDeclarations {
    pub depends: BTreeMap<String, Value>,
    pub recommends: BTreeMap<String, Value>,
    pub suggests: BTreeMap<String, Value>,
    pub breaks: BTreeMap<String, Value>,
    pub conflicts: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
pub struct Entrypoint {
    /// None means this reader kept, but could not interpret, the entry.
    pub value: Option<String>,
    pub adapter: Option<String>,
    pub environment: Option<String>,
    pub raw: Value,
}

#[derive(Clone, Debug)]
pub struct MixinConfig {
    pub file: Option<String>,
    pub environment: Option<String>,
    pub raw: Value,
}

#[derive(Clone, Debug)]
pub struct ModIdConflict {
    pub id: String,
    /// Distinct JAR contents that use this ID. No version is selected.
    pub alternatives: Vec<ModAlternative>,
}

#[derive(Clone, Debug)]
pub struct ModAlternative {
    pub version: String,
    pub sources: Vec<String>,
    pub content_sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DependencyKind {
    Depends,
    Recommends,
    Suggests,
    Breaks,
    Conflicts,
}

#[derive(Clone, Debug)]
pub struct DependencyFact {
    pub from_mod: String,
    pub dependency_id: String,
    pub kind: DependencyKind,
    /// Kept as JSON because Fabric supports more than one requirement shape.
    pub requirement: Value,
    /// Versions with this ID in the scanned JARs.
    pub candidates_in_scan: Vec<String>,
    /// Scanned versions that satisfy the declaration, when the declaration is valid.
    pub matching_candidates_in_scan: Vec<String>,
    /// Why matching was unavailable when the declaration was malformed.
    pub version_match_error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum InspectionWarning {
    RawControlCharactersInString {
        source: String,
        count: usize,
    },
    MissingSchemaVersion {
        source: String,
    },
    UnsupportedSchemaVersion {
        source: String,
        version: u32,
    },
    UnhandledMetadataField {
        source: String,
        field: String,
    },
    UnparsedEntrypoint {
        source: String,
        group: String,
        index: usize,
    },
    UnparsedMixin {
        source: String,
        index: usize,
    },
}

#[derive(Debug)]
pub struct JarFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct DirectoryScanReport {
    pub inspection: InspectionReport,
    /// JARs that could not be inspected. Other files in the folder are still reported.
    pub failures: Vec<JarFailure>,
    /// Set when a shared scan budget stopped the directory walk.
    pub budget_exhausted: Option<BudgetExhaustion>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetExhaustion {
    pub archive: String,
    pub limit: &'static str,
    pub requested: u64,
    pub maximum: u64,
}

#[derive(Debug)]
pub enum InspectError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    InvalidArchive {
        archive: String,
        message: String,
    },
    MissingManifest {
        archive: String,
    },
    InvalidMetadata {
        archive: String,
        message: String,
    },
    UnsafeArchivePath {
        archive: String,
        field: String,
        path: String,
        reason: String,
    },
    MissingArchiveEntry {
        archive: String,
        field: String,
        path: String,
    },
    DuplicateArchiveEntry {
        archive: String,
        path: String,
    },
    DuplicateNestedJarPath {
        archive: String,
        path: String,
    },
    DuplicateTopLevelModId {
        id: String,
        first: String,
        second: String,
    },
    LimitExceeded {
        archive: String,
        limit: &'static str,
        actual: u64,
        maximum: u64,
    },
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "could not read {}: {source}", path.display()),
            Self::InvalidArchive { archive, message } => {
                write!(f, "invalid JAR {archive}: {message}")
            }
            Self::MissingManifest { archive } => write!(f, "JAR {archive} has no {MANIFEST}"),
            Self::InvalidMetadata { archive, message } => {
                write!(f, "invalid {MANIFEST} in {archive}: {message}")
            }
            Self::UnsafeArchivePath {
                archive,
                field,
                path,
                reason,
            } => {
                write!(f, "unsafe {field} path {path:?} in {archive}: {reason}")
            }
            Self::MissingArchiveEntry {
                archive,
                field,
                path,
            } => {
                write!(f, "{field} path {path:?} is missing from {archive}")
            }
            Self::DuplicateArchiveEntry { archive, path } => {
                write!(
                    f,
                    "JAR {archive} contains the same ZIP path more than once: {path:?}"
                )
            }
            Self::DuplicateNestedJarPath { archive, path } => {
                write!(
                    f,
                    "{MANIFEST} in {archive} lists nested JAR path {path:?} more than once"
                )
            }
            Self::DuplicateTopLevelModId { id, first, second } => {
                write!(
                    f,
                    "top-level mod ID {id:?} is declared by both {first} and {second}"
                )
            }
            Self::LimitExceeded {
                archive,
                limit,
                actual,
                maximum,
            } => {
                write!(
                    f,
                    "JAR {archive} exceeds {limit}: {actual} (limit {maximum})"
                )
            }
        }
    }
}

impl Error for InspectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Inspect one mod JAR and its nested JARs.
pub fn inspect_jar(path: impl AsRef<Path>) -> Result<InspectionReport, InspectError> {
    inspect_jars([path.as_ref()], Limits::default())
}

/// Inspect one mod JAR using caller-supplied resource limits.
pub fn inspect_jar_with_limits(
    path: impl AsRef<Path>,
    limits: Limits,
) -> Result<InspectionReport, InspectError> {
    inspect_jars([path.as_ref()], limits)
}

/// Inspect a chosen set of JARs. Duplicate top-level mod IDs are errors.
pub fn inspect_jars<I, P>(paths: I, limits: Limits) -> Result<InspectionReport, InspectError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut report = InspectionReport::default();
    let mut budget = WorkBudget::default();
    for path in paths {
        let path = path.as_ref();
        let single = inspect_one(path, &limits, &mut budget)?;
        merge_report(&mut report, single)?;
    }
    apply_work_totals(&mut report, &budget);
    rebuild_facts(&mut report);
    Ok(report)
}

/// Inspect the JAR files directly in a folder. Invalid files do not hide valid files.
pub fn inspect_directory(path: impl AsRef<Path>) -> Result<DirectoryScanReport, InspectError> {
    inspect_directory_with_limits(path, Limits::default())
}

/// Inspect a folder using caller-supplied resource limits.
pub fn inspect_directory_with_limits(
    path: impl AsRef<Path>,
    limits: Limits,
) -> Result<DirectoryScanReport, InspectError> {
    let root = path.as_ref();
    let entries = fs::read_dir(root).map_err(|source| InspectError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| InspectError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let kind = entry.file_type().map_err(|source| InspectError::Io {
            path: entry.path(),
            source,
        })?;
        if kind.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
        {
            paths.push(entry.path());
            if paths.len() > limits.max_archives {
                return Err(limit_error(
                    root.display().to_string(),
                    "top-level JAR count",
                    paths.len() as u64,
                    limits.max_archives as u64,
                ));
            }
        }
    }
    paths.sort();

    let mut report = InspectionReport::default();
    let mut failures = Vec::new();
    let mut budget_exhausted = None;
    let mut budget = WorkBudget::default();
    for path in paths {
        match inspect_one(&path, &limits, &mut budget)
            .and_then(|single| merge_report(&mut report, single))
        {
            Ok(()) => {}
            Err(error) => {
                budget_exhausted = shared_budget_exhaustion(&error);
                failures.push(JarFailure {
                    path,
                    message: error.to_string(),
                });
                if budget_exhausted.is_some() {
                    break;
                }
            }
        }
    }
    apply_work_totals(&mut report, &budget);
    rebuild_facts(&mut report);
    Ok(DirectoryScanReport {
        inspection: report,
        failures,
        budget_exhausted,
    })
}

fn inspect_one(
    path: &Path,
    limits: &Limits,
    budget: &mut WorkBudget,
) -> Result<InspectionReport, InspectError> {
    let source = path.display().to_string();
    let mut session = Session::new(limits.clone(), budget);
    session.note_archive(&source, 0)?;
    let mut file = File::open(path).map_err(|source| InspectError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let length = file
        .metadata()
        .map_err(|source| InspectError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if length > limits.max_root_jar_bytes {
        return Err(limit_error(
            source,
            "root JAR bytes",
            length,
            limits.max_root_jar_bytes,
        ));
    }
    let hash = hash_reader(&mut file, &source, limits.max_root_jar_bytes)?;
    file.seek(io::SeekFrom::Start(0))
        .map_err(|source| InspectError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    validate_unique_zip_names(&mut file, &source, limits.max_archive_entries)?;
    let mut archive = ZipArchive::new(file).map_err(|error| invalid_archive(&source, error))?;
    session.visit(&mut archive, &source, hash, 0, true)?;
    Ok(session.report)
}

#[derive(Default)]
struct WorkBudget {
    archives_read: usize,
    nested_jar_bytes_read: u64,
    manifest_bytes_read: u64,
}

struct Session<'a> {
    limits: Limits,
    report: InspectionReport,
    by_hash: HashMap<[u8; 32], Vec<(usize, String)>>,
    by_id: HashMap<String, Vec<usize>>,
    budget: &'a mut WorkBudget,
}

impl<'a> Session<'a> {
    fn new(limits: Limits, budget: &'a mut WorkBudget) -> Self {
        Self {
            limits,
            report: InspectionReport::default(),
            by_hash: HashMap::new(),
            by_id: HashMap::new(),
            budget,
        }
    }

    fn note_archive(&mut self, source: &str, depth: usize) -> Result<(), InspectError> {
        if depth > self.limits.max_nesting_depth {
            return Err(limit_error(
                source.to_owned(),
                "nested JAR depth",
                depth as u64,
                self.limits.max_nesting_depth as u64,
            ));
        }
        let count = self.budget.archives_read.saturating_add(1);
        if count > self.limits.max_archives {
            return Err(limit_error(
                source.to_owned(),
                "archive count",
                count as u64,
                self.limits.max_archives as u64,
            ));
        }
        self.budget.archives_read = count;
        self.report.archives_read = self.report.archives_read.saturating_add(1);
        Ok(())
    }

    fn visit<R: Read + io::Seek>(
        &mut self,
        archive: &mut ZipArchive<R>,
        source: &str,
        hash: [u8; 32],
        depth: usize,
        top_level: bool,
    ) -> Result<Vec<(usize, String)>, InspectError> {
        if let Some(cached) = self.by_hash.get(&hash).cloned() {
            if top_level {
                if let Some((index, _)) = cached.iter().find(|(_, suffix)| suffix.is_empty()) {
                    if let Some(first) = self.report.mods[*index].top_level_sources.first() {
                        return Err(InspectError::DuplicateTopLevelModId {
                            id: self.report.mods[*index].id.clone(),
                            first: first.clone(),
                            second: source.to_owned(),
                        });
                    }
                }
            }
            for (index, suffix) in &cached {
                let alias = format!("{source}{suffix}");
                if !self.report.mods[*index].sources.contains(&alias) {
                    self.report.mods[*index].sources.push(alias);
                }
                if top_level && suffix.is_empty() {
                    self.report.mods[*index]
                        .top_level_sources
                        .push(source.to_owned());
                }
            }
            return Ok(cached);
        }

        if archive.len() > self.limits.max_archive_entries {
            return Err(limit_error(
                source.to_owned(),
                "ZIP entry count",
                archive.len() as u64,
                self.limits.max_archive_entries as u64,
            ));
        }
        let mut names = HashSet::with_capacity(archive.len());
        for index in 0..archive.len() {
            let name = archive
                .by_index(index)
                .map_err(|error| invalid_archive(source, error))?
                .name()
                .to_owned();
            if !names.insert(name.clone()) {
                return Err(InspectError::DuplicateArchiveEntry {
                    archive: source.to_owned(),
                    path: name,
                });
            }
        }

        let manifest = read_manifest(
            archive,
            &names,
            source,
            &self.limits,
            &mut self.report,
            self.budget,
        )?;
        let mod_index = self.add_mod(manifest, source, hash, top_level)?;
        let mut subtree = vec![(mod_index, String::new())];
        let mut declared = HashSet::new();
        for nested_path in self.report.mods[mod_index].nested_jars.clone() {
            validate_archive_path(source, "jars", &nested_path)?;
            if !declared.insert(nested_path.clone()) {
                return Err(InspectError::DuplicateNestedJarPath {
                    archive: source.to_owned(),
                    path: nested_path,
                });
            }
            let Some(index) = names.get(&nested_path) else {
                return Err(InspectError::MissingArchiveEntry {
                    archive: source.to_owned(),
                    field: "nested JAR".into(),
                    path: nested_path,
                });
            };
            let nested_source = format!("{source}!{nested_path}");
            self.note_archive(&nested_source, depth + 1)?;
            let mut entry = archive
                .by_name(index)
                .map_err(|error| invalid_archive(source, error))?;
            if entry.is_dir() {
                return Err(InspectError::MissingArchiveEntry {
                    archive: source.to_owned(),
                    field: "nested JAR".into(),
                    path: nested_path,
                });
            }
            let nested_size = entry.size();
            if nested_size > self.limits.max_nested_jar_bytes {
                return Err(limit_error(
                    source.to_owned(),
                    "nested JAR bytes",
                    nested_size,
                    self.limits.max_nested_jar_bytes,
                ));
            }
            let total = self
                .budget
                .nested_jar_bytes_read
                .saturating_add(nested_size);
            if total > self.limits.max_total_nested_jar_bytes {
                return Err(limit_error(
                    source.to_owned(),
                    "total nested JAR bytes",
                    total,
                    self.limits.max_total_nested_jar_bytes,
                ));
            }
            self.budget.nested_jar_bytes_read = total;
            self.report.nested_jar_bytes_read = self
                .report
                .nested_jar_bytes_read
                .saturating_add(nested_size);
            let mut bytes = Vec::with_capacity(nested_size as usize);
            entry
                .by_ref()
                .take(self.limits.max_nested_jar_bytes.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|error| invalid_archive(source, error))?;
            if bytes.len() as u64 != nested_size {
                return Err(InspectError::InvalidArchive {
                    archive: source.to_owned(),
                    message: format!("nested JAR {nested_path:?} has a short read"),
                });
            }
            let hash: [u8; 32] = Sha256::digest(&bytes).into();
            let mut cursor = Cursor::new(bytes);
            validate_unique_zip_names(
                &mut cursor,
                &nested_source,
                self.limits.max_archive_entries,
            )?;
            cursor.set_position(0);
            let mut nested_archive =
                ZipArchive::new(cursor).map_err(|error| invalid_archive(&nested_source, error))?;
            let child = self.visit(&mut nested_archive, &nested_source, hash, depth + 1, false)?;
            for (index, suffix) in child {
                subtree.push((index, format!("!{nested_path}{suffix}")));
            }
        }
        self.by_hash.insert(hash, subtree.clone());
        Ok(subtree)
    }

    fn add_mod(
        &mut self,
        manifest: ParsedManifest,
        source: &str,
        hash: [u8; 32],
        top_level: bool,
    ) -> Result<usize, InspectError> {
        if top_level {
            if let Some(previous) = self.by_id.get(&manifest.id).and_then(|indices| {
                indices
                    .iter()
                    .find_map(|index| self.report.mods[*index].top_level_sources.first())
            }) {
                return Err(InspectError::DuplicateTopLevelModId {
                    id: manifest.id,
                    first: previous.clone(),
                    second: source.to_owned(),
                });
            }
        }
        let index = self.report.mods.len();
        self.by_id
            .entry(manifest.id.clone())
            .or_default()
            .push(index);
        let mut top_level_sources = Vec::new();
        if top_level {
            top_level_sources.push(source.to_owned());
        }
        self.report.mods.push(ModInfo {
            id: manifest.id,
            name: manifest.name,
            version: manifest.version,
            schema_version: manifest.schema_version,
            environment: manifest.environment,
            provides: manifest.provides,
            dependencies: manifest.dependencies,
            entrypoints: manifest.entrypoints,
            mixins: manifest.mixins,
            access_widener: manifest.access_widener,
            nested_jars: manifest.nested_jars,
            sources: vec![source.to_owned()],
            top_level_sources,
            content_sha256: hex(&hash),
            unhandled_metadata: manifest.unhandled_metadata,
        });
        self.report.warnings.extend(manifest.warnings);
        Ok(index)
    }
}

struct ParsedManifest {
    id: String,
    name: Option<String>,
    version: String,
    schema_version: Option<u32>,
    environment: Option<String>,
    provides: Vec<String>,
    dependencies: DependencyDeclarations,
    entrypoints: BTreeMap<String, Vec<Entrypoint>>,
    mixins: Vec<MixinConfig>,
    access_widener: Option<String>,
    nested_jars: Vec<String>,
    unhandled_metadata: BTreeMap<String, Value>,
    warnings: Vec<InspectionWarning>,
}

fn read_manifest<R: Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    names: &HashSet<String>,
    source: &str,
    limits: &Limits,
    report: &mut InspectionReport,
    budget: &mut WorkBudget,
) -> Result<ParsedManifest, InspectError> {
    let mut entry = archive
        .by_name(MANIFEST)
        .map_err(|_| InspectError::MissingManifest {
            archive: source.to_owned(),
        })?;
    let size = entry.size();
    if size > limits.max_manifest_bytes {
        return Err(limit_error(
            source.to_owned(),
            "manifest bytes",
            size,
            limits.max_manifest_bytes,
        ));
    }
    let total = budget.manifest_bytes_read.saturating_add(size);
    if total > limits.max_total_manifest_bytes {
        return Err(limit_error(
            source.to_owned(),
            "total manifest bytes",
            total,
            limits.max_total_manifest_bytes,
        ));
    }
    budget.manifest_bytes_read = total;
    report.manifest_bytes_read = report.manifest_bytes_read.saturating_add(size);
    let mut bytes = Vec::with_capacity(size as usize);
    entry
        .by_ref()
        .take(limits.max_manifest_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| invalid_archive(source, error))?;
    if bytes.len() as u64 != size {
        return Err(InspectError::InvalidArchive {
            archive: source.to_owned(),
            message: format!("{MANIFEST} has a short read"),
        });
    }
    let (value, raw_controls) =
        parse_fabric_json(&bytes).map_err(|message| InspectError::InvalidMetadata {
            archive: source.to_owned(),
            message,
        })?;
    let Some(mut object) = value.as_object().cloned() else {
        return Err(InspectError::InvalidMetadata {
            archive: source.to_owned(),
            message: "the root must be an object".into(),
        });
    };
    let mut warnings = Vec::new();
    if raw_controls > 0 {
        warnings.push(InspectionWarning::RawControlCharactersInString {
            source: source.to_owned(),
            count: raw_controls,
        });
    }

    let schema_version = match object.remove("schemaVersion") {
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    metadata_error(source, "schemaVersion must be a non-negative integer")
                })?,
        ),
        None => {
            warnings.push(InspectionWarning::MissingSchemaVersion {
                source: source.to_owned(),
            });
            None
        }
    };
    if let Some(version) = schema_version {
        if version != 1 {
            warnings.push(InspectionWarning::UnsupportedSchemaVersion {
                source: source.to_owned(),
                version,
            });
        }
    }
    let id = required_string(&mut object, "id", source)?;
    let version = required_string(&mut object, "version", source)?;
    let name = optional_string(&mut object, "name", source)?;
    let environment = optional_string(&mut object, "environment", source)?;
    let provides = take_string_list(&mut object, "provides", source)?;
    let dependencies = parse_dependencies(&mut object, source)?;
    let entrypoints = parse_entrypoints(&mut object, source, &mut warnings)?;
    let mixins = parse_mixins(&mut object, source, names, &mut warnings)?;
    let access_widener = optional_string(&mut object, "accessWidener", source)?;
    let nested_jars = parse_nested_jars(&mut object, source, &mut warnings)?;

    for nested in &nested_jars {
        validate_archive_path(source, "jars", nested)?;
        if !names.contains(nested) {
            return Err(InspectError::MissingArchiveEntry {
                archive: source.to_owned(),
                field: "nested JAR".into(),
                path: nested.clone(),
            });
        }
    }
    if let Some(path) = &access_widener {
        validate_archive_path(source, "accessWidener", path)?;
        if !names.contains(path) {
            return Err(InspectError::MissingArchiveEntry {
                archive: source.to_owned(),
                field: "accessWidener".into(),
                path: path.clone(),
            });
        }
    }

    let known_but_unhandled = [
        "description",
        "authors",
        "contributors",
        "contact",
        "license",
        "icon",
        "languageAdapters",
        "custom",
        "$schema",
    ];
    if object.contains_key("languageAdapters") {
        warnings.push(InspectionWarning::UnhandledMetadataField {
            source: source.to_owned(),
            field: "languageAdapters".into(),
        });
    }
    for field in object.keys() {
        if !known_but_unhandled.contains(&field.as_str()) {
            warnings.push(InspectionWarning::UnhandledMetadataField {
                source: source.to_owned(),
                field: field.clone(),
            });
        }
    }
    Ok(ParsedManifest {
        id,
        name,
        version,
        schema_version,
        environment,
        provides,
        dependencies,
        entrypoints,
        mixins,
        access_widener,
        nested_jars,
        unhandled_metadata: object.into_iter().collect(),
        warnings,
    })
}

/// Match Fabric Loader's reader: raw control characters are accepted inside strings, but the
/// surrounding JSON remains strict. Escapes are temporary and decode to the original characters.
fn parse_fabric_json(bytes: &[u8]) -> Result<(Value, usize), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("metadata is not UTF-8: {error}"))?;
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut controls = 0;
    for byte in text.bytes() {
        if in_string {
            if escaped {
                normalized.push(byte);
                escaped = false;
            } else if byte == b'\\' {
                normalized.push(byte);
                escaped = true;
            } else if byte == b'"' {
                normalized.push(byte);
                in_string = false;
            } else if byte < 0x20 {
                normalized.extend_from_slice(format!("\\u{byte:04x}").as_bytes());
                controls += 1;
            } else {
                normalized.push(byte);
            }
        } else {
            if byte == b'"' {
                in_string = true;
            }
            normalized.push(byte);
        }
    }
    serde_json::from_slice(&normalized)
        .map(|value| (value, controls))
        .map_err(|error| format!("{error}; reported location is in the temporary normalized text"))
}

fn required_string(
    object: &mut Map<String, Value>,
    field: &str,
    source: &str,
) -> Result<String, InspectError> {
    match object.remove(field) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value),
        Some(_) => Err(metadata_error(
            source,
            &format!("{field} must be a non-empty string"),
        )),
        None => Err(metadata_error(
            source,
            &format!("missing required field {field:?}"),
        )),
    }
}

fn optional_string(
    object: &mut Map<String, Value>,
    field: &str,
    source: &str,
) -> Result<Option<String>, InspectError> {
    match object.remove(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(metadata_error(source, &format!("{field} must be a string"))),
    }
}

fn take_string_list(
    object: &mut Map<String, Value>,
    field: &str,
    source: &str,
) -> Result<Vec<String>, InspectError> {
    match object.remove(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    metadata_error(source, &format!("{field}[{index}] must be a string"))
                })
            })
            .collect(),
        Some(_) => Err(metadata_error(source, &format!("{field} must be an array"))),
    }
}

fn parse_dependencies(
    object: &mut Map<String, Value>,
    source: &str,
) -> Result<DependencyDeclarations, InspectError> {
    fn category(
        object: &mut Map<String, Value>,
        field: &str,
        source: &str,
    ) -> Result<BTreeMap<String, Value>, InspectError> {
        match object.remove(field) {
            None => Ok(BTreeMap::new()),
            Some(Value::Object(values)) => Ok(values.into_iter().collect()),
            Some(_) => Err(metadata_error(
                source,
                &format!("{field} must be an object"),
            )),
        }
    }
    Ok(DependencyDeclarations {
        depends: category(object, "depends", source)?,
        recommends: category(object, "recommends", source)?,
        suggests: category(object, "suggests", source)?,
        breaks: category(object, "breaks", source)?,
        conflicts: category(object, "conflicts", source)?,
    })
}

fn parse_entrypoints(
    object: &mut Map<String, Value>,
    source: &str,
    warnings: &mut Vec<InspectionWarning>,
) -> Result<BTreeMap<String, Vec<Entrypoint>>, InspectError> {
    let Some(value) = object.remove("entrypoints") else {
        return Ok(BTreeMap::new());
    };
    let Some(groups) = value.as_object() else {
        return Err(metadata_error(source, "entrypoints must be an object"));
    };
    let mut result = BTreeMap::new();
    for (group, entries) in groups {
        let items = match entries {
            Value::Array(items) => items.iter().collect::<Vec<_>>(),
            other => vec![other],
        };
        let mut parsed = Vec::new();
        for (index, raw) in items.into_iter().enumerate() {
            let (value, adapter, environment) = match raw {
                Value::String(value) => (Some(value.clone()), None, None),
                Value::Object(fields) => (
                    fields
                        .get("value")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    fields
                        .get("adapter")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    fields
                        .get("environment")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ),
                _ => (None, None, None),
            };
            let malformed = value.is_none()
                || ["adapter", "environment"]
                    .iter()
                    .any(|key| raw.get(key).is_some_and(|value| !value.is_string()));
            if malformed {
                warnings.push(InspectionWarning::UnparsedEntrypoint {
                    source: source.to_owned(),
                    group: group.clone(),
                    index,
                });
            }
            parsed.push(Entrypoint {
                value,
                adapter,
                environment,
                raw: raw.clone(),
            });
        }
        result.insert(group.clone(), parsed);
    }
    Ok(result)
}

fn parse_mixins(
    object: &mut Map<String, Value>,
    source: &str,
    names: &HashSet<String>,
    warnings: &mut Vec<InspectionWarning>,
) -> Result<Vec<MixinConfig>, InspectError> {
    let Some(value) = object.remove("mixins") else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(metadata_error(source, "mixins must be an array"));
    };
    let mut result = Vec::with_capacity(items.len());
    for (index, raw) in items.iter().enumerate() {
        let (file, environment) = match raw {
            Value::String(file) => (Some(file.clone()), None),
            Value::Object(fields) => (
                fields
                    .get("config")
                    .or_else(|| fields.get("file"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                fields
                    .get("environment")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ),
            _ => (None, None),
        };
        if let Some(file) = &file {
            validate_archive_path(source, "mixins", file)?;
            if !names.contains(file) {
                return Err(InspectError::MissingArchiveEntry {
                    archive: source.to_owned(),
                    field: "mixins".into(),
                    path: file.clone(),
                });
            }
        } else {
            warnings.push(InspectionWarning::UnparsedMixin {
                source: source.to_owned(),
                index,
            });
        }
        result.push(MixinConfig {
            file,
            environment,
            raw: raw.clone(),
        });
    }
    Ok(result)
}

fn parse_nested_jars(
    object: &mut Map<String, Value>,
    source: &str,
    warnings: &mut Vec<InspectionWarning>,
) -> Result<Vec<String>, InspectError> {
    let Some(value) = object.remove("jars") else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(metadata_error(source, "jars must be an array"));
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let Some(fields) = item.as_object() else {
                return Err(metadata_error(
                    source,
                    &format!("jars[{index}] must be an object"),
                ));
            };
            for field in fields.keys() {
                if field != "file" {
                    warnings.push(InspectionWarning::UnhandledMetadataField {
                        source: source.to_owned(),
                        field: format!("jars[{index}].{field}"),
                    });
                }
            }
            fields
                .get("file")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    metadata_error(
                        source,
                        &format!("jars[{index}].file must be a non-empty string"),
                    )
                })
        })
        .collect()
}

fn validate_archive_path(source: &str, field: &str, path: &str) -> Result<(), InspectError> {
    let unsafe_reason = if path.is_empty() {
        Some("the path is empty")
    } else if path.starts_with('/') || path.starts_with('\\') {
        Some("absolute paths are not allowed")
    } else if path.contains('\\') || path.contains(':') || path.chars().any(char::is_control) {
        Some("backslashes, drive prefixes, and control characters are not allowed")
    } else if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        Some("empty, dot, and parent path components are not allowed")
    } else {
        None
    };
    if let Some(reason) = unsafe_reason {
        return Err(InspectError::UnsafeArchivePath {
            archive: source.to_owned(),
            field: field.to_owned(),
            path: path.to_owned(),
            reason: reason.to_owned(),
        });
    }
    Ok(())
}

fn merge_report(
    target: &mut InspectionReport,
    incoming: InspectionReport,
) -> Result<(), InspectError> {
    let source = incoming
        .mods
        .first()
        .and_then(|module| module.top_level_sources.first())
        .cloned()
        .unwrap_or_default();
    if let Some(root) = incoming
        .mods
        .iter()
        .find(|module| module.top_level_sources.contains(&source))
    {
        if let Some(first) = target
            .mods
            .iter()
            .find(|module| module.id == root.id)
            .and_then(|module| module.top_level_sources.first())
        {
            return Err(InspectError::DuplicateTopLevelModId {
                id: root.id.clone(),
                first: first.clone(),
                second: source,
            });
        }
    }
    target.warnings.extend(incoming.warnings);
    for mut module in incoming.mods {
        if let Some(existing) = target
            .mods
            .iter_mut()
            .find(|existing| existing.content_sha256 == module.content_sha256)
        {
            for source in module.sources.drain(..) {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                }
            }
            for source in module.top_level_sources.drain(..) {
                if !existing.top_level_sources.contains(&source) {
                    existing.top_level_sources.push(source);
                }
            }
        } else {
            target.mods.push(module);
        }
    }
    Ok(())
}

fn apply_work_totals(report: &mut InspectionReport, budget: &WorkBudget) {
    report.archives_read = budget.archives_read;
    report.nested_jar_bytes_read = budget.nested_jar_bytes_read;
    report.manifest_bytes_read = budget.manifest_bytes_read;
}

fn shared_budget_exhaustion(error: &InspectError) -> Option<BudgetExhaustion> {
    let InspectError::LimitExceeded {
        archive,
        limit,
        actual,
        maximum,
    } = error
    else {
        return None;
    };
    if !matches!(
        *limit,
        "archive count" | "total nested JAR bytes" | "total manifest bytes"
    ) {
        return None;
    }
    Some(BudgetExhaustion {
        archive: archive.clone(),
        limit,
        requested: *actual,
        maximum: *maximum,
    })
}

fn rebuild_facts(report: &mut InspectionReport) {
    let mut by_id = HashMap::<&str, Vec<&str>>::new();
    for module in &report.mods {
        by_id.entry(&module.id).or_default().push(&module.version);
    }
    let mut alternatives = by_id
        .iter()
        .filter(|(_, versions)| versions.len() > 1)
        .map(|(id, _)| ModIdConflict {
            id: (*id).to_owned(),
            alternatives: report
                .mods
                .iter()
                .filter(|module| module.id == **id)
                .map(|module| ModAlternative {
                    version: module.version.clone(),
                    sources: module.sources.clone(),
                    content_sha256: module.content_sha256.clone(),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    alternatives.sort_by(|left, right| left.id.cmp(&right.id));
    report.mod_id_conflicts = alternatives;

    let mut facts = Vec::new();
    for module in &report.mods {
        for (kind, values) in [
            (DependencyKind::Depends, &module.dependencies.depends),
            (DependencyKind::Recommends, &module.dependencies.recommends),
            (DependencyKind::Suggests, &module.dependencies.suggests),
            (DependencyKind::Breaks, &module.dependencies.breaks),
            (DependencyKind::Conflicts, &module.dependencies.conflicts),
        ] {
            for (dependency_id, requirement) in values {
                let mut candidates_in_scan = report
                    .mods
                    .iter()
                    .filter(|candidate| candidate.id == *dependency_id)
                    .map(|candidate| candidate.version.clone())
                    .collect::<Vec<_>>();
                candidates_in_scan.sort();
                candidates_in_scan.dedup();
                let (matching_candidates_in_scan, version_match_error) =
                    match matching_versions(requirement, &candidates_in_scan) {
                        Ok(matches) => (matches, None),
                        Err(error) => (Vec::new(), Some(error.to_string())),
                    };
                facts.push(DependencyFact {
                    from_mod: module.id.clone(),
                    dependency_id: dependency_id.clone(),
                    kind,
                    requirement: requirement.clone(),
                    candidates_in_scan,
                    matching_candidates_in_scan,
                    version_match_error,
                });
            }
        }
    }
    report.dependency_facts = facts;
}

fn validate_unique_zip_names<R: Read + io::Seek>(
    reader: &mut R,
    source: &str,
    max_entries: usize,
) -> Result<(), InspectError> {
    const EOCD_SIGNATURE: u32 = 0x0605_4b50;
    const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
    const EOCD_LEN: usize = 22;
    const MAX_EOCD_TAIL: u64 = EOCD_LEN as u64 + u16::MAX as u64;

    let file_len = reader
        .seek(io::SeekFrom::End(0))
        .map_err(|error| invalid_archive(source, error))?;
    let tail_len = usize::try_from(file_len.min(MAX_EOCD_TAIL)).unwrap_or(usize::MAX);
    if tail_len < EOCD_LEN {
        return Err(invalid_archive(
            source,
            "end-of-central-directory record is missing",
        ));
    }
    let tail_start = file_len - tail_len as u64;
    let mut tail = vec![0; tail_len];
    read_at(reader, tail_start, &mut tail).map_err(|error| invalid_archive(source, error))?;
    let eocd_index = (0..=tail.len() - EOCD_LEN)
        .rev()
        .find(|index| {
            read_u32(&tail, *index) == Some(EOCD_SIGNATURE)
                && read_u16(&tail, index + 20).is_some_and(|comment_len| {
                    index + EOCD_LEN + comment_len as usize == tail.len()
                })
        })
        .ok_or_else(|| invalid_archive(source, "end-of-central-directory record is malformed"))?;
    let eocd_position = tail_start + eocd_index as u64;
    let disk_number = read_u16(&tail, eocd_index + 4).unwrap();
    let directory_disk = read_u16(&tail, eocd_index + 6).unwrap();
    let entries_on_disk = read_u16(&tail, eocd_index + 8).unwrap();
    let entries_total = read_u16(&tail, eocd_index + 10).unwrap();
    let central_size_32 = read_u32(&tail, eocd_index + 12).unwrap();
    if disk_number != 0 || directory_disk != 0 {
        return Err(invalid_archive(
            source,
            "multi-disk ZIP archives are not supported",
        ));
    }

    let locator_position = eocd_position.checked_sub(20);
    let has_zip64_locator = if let Some(position) = locator_position {
        let mut signature = [0; 4];
        read_at(reader, position, &mut signature)
            .map_err(|error| invalid_archive(source, error))?;
        read_u32(&signature, 0) == Some(ZIP64_LOCATOR_SIGNATURE)
    } else {
        false
    };
    let needs_zip64 =
        entries_total == u16::MAX || entries_on_disk == u16::MAX || central_size_32 == u32::MAX;

    let (entry_count, central_size, central_end) = if has_zip64_locator || needs_zip64 {
        let locator_position = locator_position
            .filter(|_| has_zip64_locator)
            .ok_or_else(|| invalid_archive(source, "ZIP64 locator is missing"))?;
        let mut locator = [0; 20];
        read_at(reader, locator_position, &mut locator)
            .map_err(|error| invalid_archive(source, error))?;
        if read_u32(&locator, 4) != Some(0) || read_u32(&locator, 16) != Some(1) {
            return Err(invalid_archive(
                source,
                "multi-disk ZIP64 archives are not supported",
            ));
        }
        let (zip64_position, zip64_record) =
            read_zip64_end_record(reader, locator_position, source)?;
        let record_size = read_u64(&zip64_record, 4).unwrap();
        if record_size < 44 {
            return Err(invalid_archive(source, "ZIP64 end record is too short"));
        }
        (
            read_u64(&zip64_record, 32).unwrap(),
            read_u64(&zip64_record, 40).unwrap(),
            zip64_position,
        )
    } else {
        if entries_on_disk != entries_total {
            return Err(invalid_archive(
                source,
                "split ZIP archives are not supported",
            ));
        }
        (entries_total as u64, central_size_32 as u64, eocd_position)
    };

    if entry_count > max_entries as u64 {
        return Err(limit_error(
            source.to_owned(),
            "ZIP entry count",
            entry_count,
            max_entries as u64,
        ));
    }
    let central_start = central_end
        .checked_sub(central_size)
        .ok_or_else(|| invalid_archive(source, "central directory size is out of range"))?;
    let file_len = reader
        .seek(io::SeekFrom::End(0))
        .map_err(|error| invalid_archive(source, error))?;
    if central_end > file_len {
        return Err(invalid_archive(
            source,
            "central directory extends beyond the archive",
        ));
    }

    let mut cursor = central_start;
    let mut names = HashSet::with_capacity(entry_count as usize);
    for _ in 0..entry_count {
        let mut header = [0; 46];
        if cursor.saturating_add(header.len() as u64) > central_end {
            return Err(invalid_archive(
                source,
                "central directory entry is truncated",
            ));
        }
        read_at(reader, cursor, &mut header).map_err(|error| invalid_archive(source, error))?;
        if read_u32(&header, 0) != Some(0x0201_4b50) {
            return Err(invalid_archive(
                source,
                "central directory entry signature is invalid",
            ));
        }
        let name_len = read_u16(&header, 28).unwrap() as u64;
        let extra_len = read_u16(&header, 30).unwrap() as u64;
        let comment_len = read_u16(&header, 32).unwrap() as u64;
        let next = cursor
            .checked_add(46)
            .and_then(|position| position.checked_add(name_len))
            .and_then(|position| position.checked_add(extra_len))
            .and_then(|position| position.checked_add(comment_len))
            .ok_or_else(|| invalid_archive(source, "central directory entry is too large"))?;
        if next > central_end {
            return Err(invalid_archive(
                source,
                "central directory entry is truncated",
            ));
        }
        let name_len = usize::try_from(name_len)
            .map_err(|_| invalid_archive(source, "central directory filename is too large"))?;
        let mut name = vec![0; name_len];
        read_at(reader, cursor + 46, &mut name).map_err(|error| invalid_archive(source, error))?;
        if !names.insert(name.clone()) {
            return Err(InspectError::DuplicateArchiveEntry {
                archive: source.to_owned(),
                path: String::from_utf8_lossy(&name).into_owned(),
            });
        }
        cursor = next;
    }
    Ok(())
}

fn read_zip64_end_record<R: Read + io::Seek>(
    reader: &mut R,
    locator_position: u64,
    source: &str,
) -> Result<(u64, [u8; 56]), InspectError> {
    const SIGNATURE: u32 = 0x0606_4b50;
    const SEARCH_LIMIT: u64 = 1024 * 1024;
    let search_start = locator_position.saturating_sub(SEARCH_LIMIT);
    let mut bytes = vec![0; (locator_position - search_start) as usize];
    read_at(reader, search_start, &mut bytes).map_err(|error| invalid_archive(source, error))?;
    for index in (0..=bytes.len().saturating_sub(12)).rev() {
        if read_u32(&bytes, index) != Some(SIGNATURE) {
            continue;
        }
        let Some(record_size) = read_u64(&bytes, index + 4) else {
            continue;
        };
        let position = search_start + index as u64;
        if position
            .checked_add(12)
            .and_then(|end| end.checked_add(record_size))
            != Some(locator_position)
        {
            continue;
        }
        let mut record = [0; 56];
        read_at(reader, position, &mut record).map_err(|error| invalid_archive(source, error))?;
        return Ok((position, record));
    }
    Err(invalid_archive(
        source,
        "ZIP64 end record could not be found near its locator",
    ))
}

fn read_at<R: Read + io::Seek>(reader: &mut R, position: u64, bytes: &mut [u8]) -> io::Result<()> {
    reader.seek(io::SeekFrom::Start(position))?;
    reader.read_exact(bytes)
}

fn read_u16(bytes: &[u8], position: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(position..position + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], position: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(position..position + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], position: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(position..position + 8)?.try_into().ok()?,
    ))
}

fn hash_reader(
    mut reader: impl Read,
    source: &str,
    max_bytes: u64,
) -> Result<[u8; 32], InspectError> {
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source_error| InspectError::Io {
                path: PathBuf::from(source),
                source: source_error,
            })?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(limit_error(
                source.to_owned(),
                "root JAR bytes",
                total,
                max_bytes,
            ));
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

fn metadata_error(source: &str, message: &str) -> InspectError {
    InspectError::InvalidMetadata {
        archive: source.to_owned(),
        message: message.to_owned(),
    }
}

fn invalid_archive(source: &str, error: impl fmt::Display) -> InspectError {
    InspectError::InvalidArchive {
        archive: source.to_owned(),
        message: error.to_string(),
    }
}

fn limit_error(source: String, limit: &'static str, actual: u64, maximum: u64) -> InspectError {
    InspectError::LimitExceeded {
        archive: source,
        limit,
        actual,
        maximum,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
