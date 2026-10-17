#[path = "conformance/mod.rs"]
mod conformance;

#[test]
fn legacy_conformance() {
    conformance::run_legacy_suite();
}
