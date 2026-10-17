use std::path::{Path, PathBuf};
use std::process::Command;

pub fn compile(directory: &Path, name: &str) -> PathBuf {
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let mut command = match std::env::var_os("RUSTC_WRAPPER").filter(|value| !value.is_empty()) {
        Some(wrapper) => {
            let mut command = Command::new(wrapper);
            command.arg(compiler);
            command
        }
        None => Command::new(compiler),
    };
    let executable = directory.join(name);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.rs"));
    let output = command
        .arg("--edition=2024")
        .arg("--crate-name")
        .arg(name)
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("failed to run fixture compiler");
    assert!(output.status.success(), "fixture {name}: {output:?}");
    executable
}
