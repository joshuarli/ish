mod executor;
mod fixture;
mod parser;
mod pattern;

use std::collections::HashSet;

use executor::{assertion_failures, execute_case};
use parser::{Case, parse_corpus};

const CORPUS: &[u8] = include_bytes!("../../check-epsh.t");
const EPSH: &str = env!("CARGO_BIN_EXE_epsh");

enum Selection {
    All,
    Id(String),
    Category(Vec<u8>),
}

#[derive(Default)]
struct Summary {
    selected: usize,
    passed: usize,
    excluded: usize,
    ignored_failure: usize,
    failed: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum CaseOutcome {
    Passed,
    IgnoredFailure,
    Failed,
}

pub fn run_legacy_suite() {
    let cases = parse_corpus(CORPUS).unwrap_or_else(|error| panic!("cannot parse check-epsh.t: {error}"));
    assert_eq!(cases.len(), 175, "the retained corpus must keep all 175 test IDs");
    let selection = selection_from_environment();
    let host = host_categories();
    let chosen = cases.iter().filter(|case| selection_matches(&selection, case)).collect::<Vec<_>>();
    assert!(!chosen.is_empty(), "conformance selector matched no case");
    let mut summary = Summary { selected: chosen.len(), ..Summary::default() };
    let mut failures = Vec::new();

    for case in chosen {
        if let Some(reason) = category_exclusion(case, &host) {
            summary.excluded += 1;
            println!("{}\texcluded\t{}", case.name, reason);
            continue;
        }
        if let Some(info) = &case.info_pre {
            println!("info v[{}]", std::str::from_utf8(info).expect("info-pre must be printable UTF-8"));
        }
        let binary = std::env::var_os("EPSH_CONFORMANCE_PROGRAM").unwrap_or_else(|| EPSH.into());
        let output = execute_case(case, &binary)
            .unwrap_or_else(|error| panic!("{}: harness failed: {error}", case.name));
        let case_failures = assertion_failures(case, &output);
        match classify_case(case.need_pass, &case_failures) {
            CaseOutcome::Passed => {
                summary.passed += 1;
                println!("{}\tpass\t{}", case.name, exit_label(&output));
            }
            CaseOutcome::IgnoredFailure => {
                summary.ignored_failure += 1;
                println!("{}\tignored-failure\t{}", case.name, exit_label(&output));
                for failure in &case_failures {
                    eprintln!("{} (ignored): {failure}; description={:?}", case.name, case.description);
                }
            }
            CaseOutcome::Failed => {
                summary.failed += 1;
                println!("{}\tfail\t{}", case.name, exit_label(&output));
                failures.extend(case_failures.iter().map(|failure| format!("{}: {failure}; description={:?}", case.name, case.description)));
            }
        }
    }

    println!(
        "conformance: selected={} passed={} excluded={} ignored-failure={} failed={}",
        summary.selected,
        summary.passed,
        summary.excluded,
        summary.ignored_failure,
        summary.failed,
    );
    if !failures.is_empty() {
        panic!("conformance failures:\n{}", failures.join("\n"));
    }
}

fn classify_case(need_pass: bool, failures: &[String]) -> CaseOutcome {
    if failures.is_empty() {
        CaseOutcome::Passed
    } else if need_pass {
        CaseOutcome::Failed
    } else {
        CaseOutcome::IgnoredFailure
    }
}

fn selection_from_environment() -> Selection {
    let id = std::env::var_os("EPSH_CONFORMANCE_ID");
    let category = std::env::var_os("EPSH_CONFORMANCE_CATEGORY");
    match (id, category) {
        (Some(_), Some(_)) => panic!("set only one of EPSH_CONFORMANCE_ID or EPSH_CONFORMANCE_CATEGORY"),
        (Some(id), None) => Selection::Id(id.into_string().expect("case ID selector must be UTF-8")),
        (None, Some(category)) => Selection::Category(
            category.into_string().expect("category selector must be UTF-8").into_bytes(),
        ),
        (None, None) => Selection::All,
    }
}

fn selection_matches(selection: &Selection, case: &Case) -> bool {
    match selection {
        Selection::All => true,
        Selection::Id(id) => case.name == *id,
        Selection::Category(category) => case.category.as_deref().is_some_and(|value| {
            split_categories(value).any(|candidate| candidate.strip_prefix(b"!").unwrap_or(candidate) == category)
        }),
    }
}

fn category_exclusion(case: &Case, host: &HashSet<Vec<u8>>) -> Option<String> {
    let Some(category) = &case.category else {
        return None;
    };
    let mut matches = false;
    for candidate in split_categories(category) {
        if let Some(excluded) = candidate.strip_prefix(b"!") {
            if host.contains(excluded) {
                return Some(format!("category excludes {}", std::str::from_utf8(excluded).expect("category names are ASCII")));
            }
            matches = true;
        } else if host.contains(candidate) {
            matches = true;
        }
    }
    if matches {
        None
    } else {
        Some(format!("category requires {}", std::str::from_utf8(category).expect("category names are ASCII")))
    }
}

fn split_categories(value: &[u8]) -> impl Iterator<Item = &[u8]> {
    value.split(|byte| *byte == b',').map(trim_ascii)
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn host_categories() -> HashSet<Vec<u8>> {
    let mut categories = HashSet::new();
    categories.insert(format!("os:{}", perl_os_name()).into_bytes());
    categories.insert(b"shell:ebcdic-no".to_vec());
    categories.insert(b"shell:ascii-yes".to_vec());
    categories
}

#[cfg(target_os = "macos")]
fn perl_os_name() -> &'static str {
    "darwin"
}

#[cfg(target_os = "windows")]
fn perl_os_name() -> &'static str {
    "MSWin32"
}

#[cfg(target_os = "linux")]
fn perl_os_name() -> &'static str {
    "linux"
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn perl_os_name() -> &'static str {
    std::env::consts::OS
}

fn exit_label(output: &executor::ProcessOutput) -> String {
    match output.status.as_ref().and_then(|status| status.code()) {
        Some(code) => format!("exit={code}"),
        None => output.status.as_ref().map_or_else(
            || "exit=unknown".to_owned(),
            |status| format!("signal={}", std::os::unix::process::ExitStatusExt::signal(status).unwrap_or_default()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{CaseOutcome, classify_case};

    #[test]
    fn need_pass_no_keeps_failures_informational() {
        let failures = vec!["expected output differed".to_owned()];
        assert_eq!(classify_case(false, &failures), CaseOutcome::IgnoredFailure);
        assert_eq!(classify_case(true, &failures), CaseOutcome::Failed);
        assert_eq!(classify_case(false, &[]), CaseOutcome::Passed);
    }
}
