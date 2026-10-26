//! Fabric's small version-rule language for `fabric.mod.json` dependencies.
//!
//! This checks a rule against versions already found in a scan. It does not pick a mod,
//! install one, or decide whether a mod can run.

use std::error::Error;
use std::fmt;

use serde_json::Value;

const MAX_RULE_BYTES: usize = 64 * 1024;
const MAX_RANGES: usize = 1024;
const MAX_TERMS: usize = 1024;
const MAX_CANDIDATES: usize = 4096;
const MAX_VERSION_BYTES: usize = 4 * 1024;
const MAX_VERSION_COMPONENTS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionPredicateError(String);

impl fmt::Display for VersionPredicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for VersionPredicateError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operator {
    Equal,
    GreaterEqual,
    LessEqual,
    Greater,
    Less,
    SameMajor,
    SameMinor,
}

#[derive(Clone, Debug)]
struct Version {
    raw: String,
    semantic: Option<SemanticVersion>,
}

#[derive(Clone, Debug)]
struct SemanticVersion {
    components: Vec<Component>,
    prerelease: Option<String>,
    build: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Component {
    Number(i32),
    Wildcard,
}

#[derive(Clone, Debug)]
struct Term {
    operator: Operator,
    version: Version,
}

#[derive(Clone, Debug)]
struct Predicate(Vec<Term>);

/// Return the scanned versions that satisfy a Fabric dependency declaration.
///
/// A string is one range; its space-separated terms are ANDed. An array of strings is
/// a list of ranges, and any matching range satisfies it. An empty array matches nothing.
pub fn matching_versions(
    requirement: &Value,
    candidates: &[String],
) -> Result<Vec<String>, VersionPredicateError> {
    if candidates.len() > MAX_CANDIDATES {
        return Err(VersionPredicateError(format!(
            "dependency matching has more than {MAX_CANDIDATES} candidate versions"
        )));
    }

    let mut rule_bytes = 0usize;
    let mut term_count = 0usize;
    let predicates = match requirement {
        Value::String(range) => {
            rule_bytes = range.len();
            term_count = count_terms(range);
            if rule_bytes > MAX_RULE_BYTES || term_count > MAX_TERMS {
                return Err(VersionPredicateError(
                    "dependency declaration exceeds the version-rule limits".to_owned(),
                ));
            }
            vec![parse_predicate(range)?]
        }
        Value::Array(ranges) => {
            if ranges.len() > MAX_RANGES {
                return Err(VersionPredicateError(format!(
                    "dependency declaration has more than {MAX_RANGES} alternative ranges"
                )));
            }
            let mut predicates = Vec::with_capacity(ranges.len());
            for range in ranges {
                let range = range.as_str().ok_or_else(|| {
                    VersionPredicateError("dependency ranges must be strings".to_owned())
                })?;
                rule_bytes = rule_bytes.saturating_add(range.len()).saturating_add(1);
                term_count = term_count.saturating_add(count_terms(range));
                if rule_bytes > MAX_RULE_BYTES || term_count > MAX_TERMS {
                    return Err(VersionPredicateError(
                        "dependency declaration exceeds the version-rule limits".to_owned(),
                    ));
                }
                predicates.push(parse_predicate(range)?);
            }
            predicates
        }
        _ => {
            return Err(VersionPredicateError(
                "dependency range must be a string or an array of strings".to_owned(),
            ));
        }
    };

    if rule_bytes > MAX_RULE_BYTES {
        return Err(VersionPredicateError(format!(
            "dependency declaration is larger than {MAX_RULE_BYTES} bytes"
        )));
    }
    if term_count > MAX_TERMS {
        return Err(VersionPredicateError(format!(
            "dependency declaration has more than {MAX_TERMS} version checks"
        )));
    }

    let mut matches = Vec::new();
    for candidate in candidates {
        let version = Version::parse(candidate, false)?;
        if predicates
            .iter()
            .any(|predicate| predicate.matches(&version))
        {
            matches.push(candidate.clone());
        }
    }
    Ok(matches)
}

/// Check one candidate against one Fabric dependency declaration.
pub fn version_matches(
    requirement: &Value,
    candidate: &str,
) -> Result<bool, VersionPredicateError> {
    let matches = matching_versions(requirement, &[candidate.to_owned()])?;
    Ok(!matches.is_empty())
}

impl Version {
    fn parse(raw: &str, allow_wildcard: bool) -> Result<Self, VersionPredicateError> {
        if raw.is_empty() {
            return Err(VersionPredicateError(
                "version must not be empty".to_owned(),
            ));
        }
        if raw.len() > MAX_VERSION_BYTES {
            return Err(VersionPredicateError(format!(
                "version text is larger than {MAX_VERSION_BYTES} bytes"
            )));
        }
        let before_build = raw.split_once('+').map_or(raw, |(version, _)| version);
        let core = before_build
            .split_once('-')
            .map_or(before_build, |(core, _)| core);
        if core.split('.').count() > MAX_VERSION_COMPONENTS {
            return Err(VersionPredicateError(format!(
                "version has more than {MAX_VERSION_COMPONENTS} components"
            )));
        }
        Ok(Self {
            raw: raw.to_owned(),
            semantic: parse_semantic(raw, allow_wildcard),
        })
    }
}

impl Predicate {
    fn matches(&self, candidate: &Version) -> bool {
        self.0.iter().all(|term| term.matches(candidate))
    }
}

impl Term {
    fn matches(&self, candidate: &Version) -> bool {
        match (&candidate.semantic, &self.version.semantic) {
            (Some(candidate), Some(reference)) => match self.operator {
                Operator::Equal => candidate.compare(reference) == std::cmp::Ordering::Equal,
                Operator::GreaterEqual => candidate.compare(reference) != std::cmp::Ordering::Less,
                Operator::LessEqual => candidate.compare(reference) != std::cmp::Ordering::Greater,
                Operator::Greater => candidate.compare(reference) == std::cmp::Ordering::Greater,
                Operator::Less => candidate.compare(reference) == std::cmp::Ordering::Less,
                Operator::SameMajor => {
                    candidate.compare(reference) != std::cmp::Ordering::Less
                        && candidate.component(0) == reference.component(0)
                }
                Operator::SameMinor => {
                    candidate.compare(reference) != std::cmp::Ordering::Less
                        && candidate.component(0) == reference.component(0)
                        && candidate.component(1) == reference.component(1)
                }
            },
            _ => match self.operator {
                // Fabric only permits a plain-string requirement to match exactly.
                // If the candidate is plain, inclusive semantic rules also compare by
                // exact text, as Fabric's version operator does.
                Operator::Greater | Operator::Less => false,
                _ => candidate.raw == self.version.raw,
            },
        }
    }
}

impl SemanticVersion {
    fn component(&self, index: usize) -> i32 {
        self.components
            .get(index)
            .copied()
            .map(|component| match component {
                Component::Number(value) => value,
                Component::Wildcard => 0,
            })
            .unwrap_or(0)
    }

    fn compare(&self, other: &Self) -> std::cmp::Ordering {
        for index in 0..self.components.len().max(other.components.len()) {
            let left = self.component(index);
            let right = other.component(index);
            match left.cmp(&right) {
                std::cmp::Ordering::Equal => {}
                different => return different,
            }
        }

        match (&self.prerelease, &other.prerelease) {
            (None, None) => std::cmp::Ordering::Equal,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(left), Some(right)) => compare_prerelease(left, right),
        }
    }
}

fn compare_prerelease(left: &str, right: &str) -> std::cmp::Ordering {
    let left = left
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let right = right
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    for (left, right) in left.iter().zip(&right) {
        let left_numeric = is_canonical_number(left);
        let right_numeric = is_canonical_number(right);
        let order = match (left_numeric, right_numeric) {
            (true, true) => left.len().cmp(&right.len()).then_with(|| left.cmp(right)),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => left.cmp(right),
        };
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn is_canonical_number(part: &str) -> bool {
    part == "0"
        || (part.starts_with(|character: char| ('1'..='9').contains(&character))
            && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn parse_semantic(raw: &str, allow_wildcard: bool) -> Option<SemanticVersion> {
    let (before_build, build) = match raw.split_once('+') {
        Some((version, build)) => (version, Some(build.to_owned())),
        None => (raw, None),
    };
    let (core, prerelease) = match before_build.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease.to_owned())),
        None => (before_build, None),
    };

    if let Some(prerelease) = &prerelease
        && !prerelease.is_empty()
        && !prerelease.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return None;
    }
    if core.is_empty() || core.starts_with('.') || core.ends_with('.') {
        return None;
    }

    let mut components = Vec::new();
    let mut first_wildcard = None;
    for piece in core.split('.') {
        if piece.is_empty() || piece.trim().is_empty() {
            return None;
        }
        if allow_wildcard && matches!(piece, "x" | "X" | "*") {
            if prerelease.is_some() {
                return None;
            }
            first_wildcard.get_or_insert(components.len());
            components.push(Component::Wildcard);
        } else {
            if components.last() == Some(&Component::Wildcard) {
                return None;
            }
            let value = piece.parse::<i32>().ok()?;
            if value < 0 {
                return None;
            }
            components.push(Component::Number(value));
        }
    }

    if let Some(index) = first_wildcard {
        if index == 0 {
            return None;
        }
        components.truncate(index + 1);
    }

    Some(SemanticVersion {
        components,
        prerelease,
        build,
    })
}

fn parse_predicate(input: &str) -> Result<Predicate, VersionPredicateError> {
    if input.len() > MAX_RULE_BYTES {
        return Err(VersionPredicateError(format!(
            "dependency declaration is larger than {MAX_RULE_BYTES} bytes"
        )));
    }
    if count_terms(input) > MAX_TERMS {
        return Err(VersionPredicateError(format!(
            "dependency declaration has more than {MAX_TERMS} version checks"
        )));
    }
    if input.is_empty() || input == "*" {
        return Ok(Predicate(Vec::new()));
    }

    let mut terms = Vec::new();
    for part in input.split(' ') {
        let part = part.trim();
        if part.is_empty() || part == "*" {
            continue;
        }

        let (mut operator, version_text) = parse_operator(part);
        let mut version = Version::parse(version_text, true)?;
        if let Some(semantic) = version.semantic.as_mut()
            && semantic.components.contains(&Component::Wildcard)
        {
            if operator != Operator::Equal {
                return Err(VersionPredicateError(
                    "wildcard ranges only allow equality or no operator".to_owned(),
                ));
            }
            let count = semantic.components.len();
            let mut lower = semantic.components[..count - 1].to_vec();
            let build = semantic.build.clone();
            let lower_semantic = SemanticVersion {
                components: lower.clone(),
                prerelease: Some(String::new()),
                build,
            };
            if count == 2 {
                operator = Operator::SameMajor;
                version = semantic_version(lower_semantic);
            } else if count == 3 {
                operator = Operator::SameMinor;
                version = semantic_version(lower_semantic);
            } else {
                terms.push(Term {
                    operator: Operator::GreaterEqual,
                    version: semantic_version(lower_semantic),
                });
                let last = lower.last_mut().ok_or_else(|| {
                    VersionPredicateError("wildcard range has no numeric prefix".to_owned())
                })?;
                let Component::Number(value) = last else {
                    return Err(VersionPredicateError(
                        "wildcard range has no numeric prefix".to_owned(),
                    ));
                };
                *value = value.wrapping_add(1);
                version = semantic_version(SemanticVersion {
                    components: lower,
                    prerelease: Some(String::new()),
                    build: None,
                });
                operator = Operator::Less;
            }
        } else if version.semantic.is_none() {
            if matches!(operator, Operator::Greater | Operator::Less) {
                return Err(VersionPredicateError(
                    "plain-string versions do not allow exclusive range operators".to_owned(),
                ));
            }
            operator = Operator::Equal;
        }

        terms.push(Term { operator, version });
    }
    Ok(Predicate(terms))
}

fn count_terms(input: &str) -> usize {
    if input.is_empty() || input == "*" {
        return 0;
    }
    input
        .split(' ')
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != "*")
        .count()
}

fn semantic_version(semantic: SemanticVersion) -> Version {
    let mut raw = semantic
        .components
        .iter()
        .map(|component| match component {
            Component::Number(value) => value.to_string(),
            Component::Wildcard => "x".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(".");
    if let Some(prerelease) = &semantic.prerelease {
        raw.push('-');
        raw.push_str(prerelease);
    }
    if let Some(build) = &semantic.build {
        raw.push('+');
        raw.push_str(build);
    }
    Version {
        raw,
        semantic: Some(semantic),
    }
}

fn parse_operator(part: &str) -> (Operator, &str) {
    for (text, operator) in [
        (">=", Operator::GreaterEqual),
        ("<=", Operator::LessEqual),
        (">", Operator::Greater),
        ("<", Operator::Less),
        ("=", Operator::Equal),
        ("~", Operator::SameMinor),
        ("^", Operator::SameMajor),
    ] {
        if let Some(version) = part.strip_prefix(text) {
            return (operator, version);
        }
    }
    (Operator::Equal, part)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CANDIDATES, MAX_RANGES, MAX_RULE_BYTES, MAX_TERMS, MAX_VERSION_BYTES,
        MAX_VERSION_COMPONENTS, matching_versions, version_matches,
    };
    use serde_json::{Value, json};

    #[test]
    fn strings_and_arrays_keep_fabric_and_or_rules() {
        assert!(version_matches(&json!(">=1.2 <2"), "1.9").unwrap());
        assert!(!version_matches(&json!(">=1.2 <2"), "2.0").unwrap());
        assert!(version_matches(&json!(["<1", ">=2 <3"]), "2.5").unwrap());
        assert!(!version_matches(&json!(["<1", ">=2 <3"]), "1.5").unwrap());
        assert!(!version_matches(&json!([]), "1.0").unwrap());
        assert!(version_matches(&json!(""), "odd-version-name").unwrap());
    }

    #[test]
    fn semantic_versions_pad_components_and_ignore_build_metadata() {
        assert!(version_matches(&json!("26.1"), "26.1.0").unwrap());
        assert!(version_matches(&json!("=1.2.3+left"), "1.2.3+right").unwrap());
        assert!(version_matches(&json!(">=1.2.3+old"), "1.2.3+new").unwrap());
        assert!(version_matches(&json!(">1.2.3-alpha.9"), "1.2.3-alpha.10").unwrap());
        assert!(!version_matches(&json!(">1.2.3-alpha.9"), "1.2.3-alpha.2").unwrap());
        assert!(version_matches(&json!("<1.2.3"), "1.2.3-rc.1").unwrap());
        assert!(!version_matches(&json!(">1.2.3"), "1.2.3-rc.1").unwrap());
        assert!(version_matches(&json!(">1.0.0-alpha.9"), "1.0.0-alpha.09").unwrap());
        assert!(!version_matches(&json!(">1.0.0-alpha.09"), "1.0.0-alpha.9").unwrap());
    }

    #[test]
    fn wildcard_forms_match_their_documented_ranges() {
        assert!(version_matches(&json!("1.x"), "1.9.4").unwrap());
        assert!(!version_matches(&json!("1.X"), "2.0").unwrap());
        assert!(version_matches(&json!("1.2.*"), "1.2.8").unwrap());
        assert!(!version_matches(&json!("1.2.x"), "1.3.0").unwrap());
        assert!(version_matches(&json!("1.2.3.x"), "1.2.3.99").unwrap());
        assert!(!version_matches(&json!("1.2.3.x"), "1.2.4.0").unwrap());
        assert!(version_matches(&json!("1.2.3.4.x"), "1.2.3.4.7").unwrap());
        assert!(version_matches(&json!("1.x"), "1.0.0-alpha.1").unwrap());
        assert!(version_matches(&json!("0.x"), "0.9.0").unwrap());
        assert!(!version_matches(&json!("0.x"), "1.0.0").unwrap());
        assert!(version_matches(&json!("^0.x"), "0.9.0").is_err());
        assert!(version_matches(&json!("~1.2"), "1.2.99").unwrap());
        assert!(!version_matches(&json!("~1.2"), "1.3.0").unwrap());
    }

    #[test]
    fn plain_versions_only_match_exact_text() {
        assert!(version_matches(&json!("snapshot-a"), "snapshot-a").unwrap());
        assert!(!version_matches(&json!("snapshot-a"), "snapshot-b").unwrap());
        assert!(version_matches(&json!(">=snapshot-a"), "snapshot-a").unwrap());
        assert!(!version_matches(&json!(">=snapshot-a"), "snapshot-b").unwrap());
        assert!(!version_matches(&json!(">=1.0"), "release-1.0").unwrap());
        assert!(version_matches(&json!("release-1.0"), "release-1.0").unwrap());
    }

    #[test]
    fn malformed_declarations_return_a_clear_error() {
        assert!(version_matches(&json!(">snapshot"), "snapshot").is_err());
        assert!(version_matches(&json!(">=1.x"), "1.2").is_err());
        assert!(version_matches(&json!(["1.0", 2]), "1.0").is_err());
        assert!(version_matches(&json!(42), "1.0").is_err());
    }

    #[test]
    fn matching_list_preserves_candidate_order_and_duplicate_entries() {
        let candidates = vec!["2.1".to_owned(), "1.9".to_owned(), "2.1".to_owned()];
        assert_eq!(
            matching_versions(&json!(">=2"), &candidates).unwrap(),
            ["2.1", "2.1"]
        );
    }

    #[test]
    fn version_rule_limits_return_errors_instead_of_silent_mismatches() {
        let oversized_rule = "a".repeat(MAX_RULE_BYTES + 1);
        assert!(version_matches(&json!(oversized_rule), "1.0").is_err());

        let too_many_ranges = Value::Array(vec![json!("*"); MAX_RANGES + 1]);
        assert!(version_matches(&too_many_ranges, "1.0").is_err());

        let too_many_terms = vec!["1"; MAX_TERMS + 1].join(" ");
        assert!(version_matches(&json!(too_many_terms), "1").is_err());

        assert!(version_matches(&json!("*"), &"1".repeat(MAX_VERSION_BYTES + 1)).is_err());

        let too_many_components = vec!["1"; MAX_VERSION_COMPONENTS + 1].join(".");
        assert!(version_matches(&json!("*"), &too_many_components).is_err());

        let too_many_candidates = vec!["1".to_owned(); MAX_CANDIDATES + 1];
        assert!(matching_versions(&json!("*"), &too_many_candidates).is_err());
    }
}
