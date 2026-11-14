//! Runs the pure step-loop planner tests (`tests/step_plan.cjs`) under real node.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]
use std::path::PathBuf;

#[test]
fn step_plan_module_passes_its_node_tests() {
    let Some(node) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("node"))
        .find(|p| p.is_file())
    else {
        assert!(
            std::env::var("CONSPECT_REQUIRE_REAL_BROWSER").as_deref() != Ok("1"),
            "node required"
        );
        eprintln!("SKIPPED step_plan test: node not found");
        return;
    };
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/step_plan.cjs");
    let out = std::process::Command::new(node)
        .arg("--test")
        .arg(&script)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
