//! Workspace automation commands.
//!
//! `cargo xtask doctor` is intentionally dependency-light so it can diagnose a newly
//! bootstrapped workspace before the product crates are built.

mod outcome;
mod scan_isolation;
mod verification;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    process::{self, Command},
};

use serde_json::Value;

use outcome::CommandError;

const REQUIRED_DIRECTORIES: &[&str] = &[
    "apps/superposition",
    "helpers/sp-plugin-worker",
    "helpers/sp-plugin-scanner",
    "crates/sp-model",
    "crates/sp-protocol",
    "crates/sp-shared-memory",
    "crates/sp-shared-memory-macos",
    "crates/sp-engine",
    "crates/sp-audio-io",
    "crates/sp-vst3",
    "crates/sp-supervisor",
    "crates/sp-session",
    "crates/sp-midi",
    "crates/sp-ui",
    "tools/xtask",
    "compatibility",
];

const REQUIRED_PATHS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    ".github/workflows/ci.yml",
    "rust-toolchain.toml",
    "deny.toml",
    "compatibility/corpus.toml",
    "apps/superposition/Cargo.toml",
    "apps/superposition/src/main.rs",
    "helpers/sp-plugin-worker/Cargo.toml",
    "helpers/sp-plugin-worker/src/main.rs",
    "helpers/sp-plugin-scanner/Cargo.toml",
    "helpers/sp-plugin-scanner/src/main.rs",
    "crates/sp-model/Cargo.toml",
    "crates/sp-model/src/lib.rs",
    "crates/sp-protocol/Cargo.toml",
    "crates/sp-protocol/src/lib.rs",
    "crates/sp-shared-memory/Cargo.toml",
    "crates/sp-shared-memory/src/lib.rs",
    "crates/sp-shared-memory-macos/Cargo.toml",
    "crates/sp-shared-memory-macos/src/lib.rs",
    "crates/sp-engine/Cargo.toml",
    "crates/sp-engine/src/lib.rs",
    "crates/sp-audio-io/Cargo.toml",
    "crates/sp-audio-io/src/lib.rs",
    "crates/sp-audio-io-macos/Cargo.toml",
    "crates/sp-audio-io-macos/src/lib.rs",
    "crates/sp-vst3/Cargo.toml",
    "crates/sp-vst3/src/lib.rs",
    "crates/sp-supervisor/Cargo.toml",
    "crates/sp-supervisor/src/lib.rs",
    "crates/sp-session/Cargo.toml",
    "crates/sp-session/src/lib.rs",
    "crates/sp-midi/Cargo.toml",
    "crates/sp-midi/src/lib.rs",
    "crates/sp-ui/Cargo.toml",
    "crates/sp-ui/src/design/tokens.rs",
    "crates/sp-ui/src/design/theme.rs",
    "tools/xtask/Cargo.toml",
    "tools/xtask/src/main.rs",
];

/// A dependency graph keyed by Cargo package name.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PackageGraph {
    edges: BTreeMap<String, BTreeSet<String>>,
}

impl PackageGraph {
    fn insert_edge(&mut self, package: impl Into<String>, dependency: impl Into<String>) {
        self.edges
            .entry(package.into())
            .or_default()
            .insert(dependency.into());
    }

    fn contains_path(&self, start: &str, target: &str) -> bool {
        let mut queue = VecDeque::from([start]);
        let mut visited = BTreeSet::new();

        while let Some(package) = queue.pop_front() {
            if !visited.insert(package) {
                continue;
            }
            if package == target {
                return true;
            }
            if let Some(dependencies) = self.edges.get(package) {
                queue.extend(dependencies.iter().map(String::as_str));
            }
        }

        false
    }

    fn direct_dependents_of(&self, package: &str) -> BTreeSet<&str> {
        self.edges
            .iter()
            .filter_map(|(dependent, dependencies)| {
                dependencies.contains(package).then_some(dependent.as_str())
            })
            .collect()
    }
}

/// The outcome of one independent doctor check.
struct CheckResult {
    name: &'static str,
    passed: bool,
    detail: String,
    action: Option<String>,
}

impl CheckResult {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: true,
            detail: detail.into(),
            action: None,
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
            action: Some(action.into()),
        }
    }

    fn print(&self) {
        let status = if self.passed { "PASS" } else { "FAIL" };
        println!("[{status}] {}: {}", self.name, self.detail);
        if let Some(action) = &self.action {
            println!("       Action: {action}");
        }
    }
}

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let exit_code = match run(&arguments) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("xtask: {error}");
            error.exit_code()
        }
    };
    if exit_code != 0 {
        process::exit(i32::from(exit_code));
    }
}

#[derive(Debug)]
enum XtaskError {
    Standard(String),
    Command(CommandError),
}

impl XtaskError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Standard(_) => 1,
            Self::Command(_) => CommandError::EXIT_CODE,
        }
    }
}

impl std::fmt::Display for XtaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Standard(error) => formatter.write_str(error),
            Self::Command(error) => error.fmt(formatter),
        }
    }
}

fn run(arguments: &[String]) -> Result<u8, XtaskError> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(XtaskError::Standard(usage()));
    };

    let verify = match command {
        "doctor" => {
            if arguments.len() != 1 {
                return Err(XtaskError::Standard(
                    "doctor does not accept arguments".to_owned(),
                ));
            }
            run_doctor(&workspace_root()).map_err(XtaskError::Standard)?;
            return Ok(0);
        }
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            return Ok(0);
        }
        "host-checker" => verification::run_host_checker,
        "compatibility" => verification::run_compatibility,
        "loopback" => verification::run_loopback,
        "click-test" => verification::run_click_test,
        "midi-timing" => verification::run_midi_timing,
        "soak" => verification::run_soak,
        "bundle" => verification::run_bundle,
        _ => {
            return Err(XtaskError::Standard(format!(
                "unknown command `{command}`\n\n{}",
                usage()
            )));
        }
    };
    verify(&workspace_root(), &arguments[1..])
        .map(|outcome| outcome.exit_code)
        .map_err(XtaskError::Command)
}

fn usage() -> String {
    format!(
        "Usage: cargo xtask <command>\n\nCommands:\n  doctor\n  {}\n  {}\n  {}\n  {}\n  {}\n  {}\n  {}",
        verification::usage_host_checker(),
        verification::usage_compatibility(),
        verification::usage_loopback(),
        verification::usage_click_test(),
        verification::usage_midi_timing(),
        verification::usage_soak(),
        verification::usage_bundle(),
    )
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask manifest has two workspace parents")
        .to_path_buf()
}

fn run_doctor(workspace_root: &Path) -> Result<(), String> {
    let mut implementation_checks = vec![
        platform_check(),
        toolchain_check(workspace_root),
        required_tooling_check(),
        required_paths_check(workspace_root),
    ];

    match cargo_metadata(workspace_root) {
        Ok(graph) => implementation_checks.push(dependency_boundary_check(&graph)),
        Err(error) => implementation_checks.push(CheckResult::fail(
            "workspace metadata",
            error,
            "Repair the listed Cargo manifests, then rerun `cargo xtask doctor`.",
        )),
    }

    println!("Implementation readiness (static checks only):");
    let failures = implementation_checks
        .iter()
        .filter(|check| !check.passed)
        .count();
    for check in &implementation_checks {
        check.print();
    }

    print_hardware_certification_status();
    print_qualification_tooling();

    if failures == 0 {
        println!("Implementation readiness passed; hardware certification remains pending.");
        Ok(())
    } else {
        Err(format!(
            "implementation readiness has {failures} failing check(s); hardware certification was not evaluated"
        ))
    }
}

fn platform_check() -> CheckResult {
    let os = env::consts::OS;
    let architecture = env::consts::ARCH;
    if os == "macos" && architecture == "aarch64" {
        CheckResult::pass("platform", "macOS/aarch64 is supported")
    } else {
        CheckResult::fail(
            "platform",
            format!("detected {os}/{architecture}; only macOS/aarch64 is supported"),
            "Use an Apple Silicon macOS host.",
        )
    }
}

fn toolchain_check(workspace_root: &Path) -> CheckResult {
    let configured = match configured_toolchain(workspace_root) {
        Ok(version) => version,
        Err(error) => {
            return CheckResult::fail(
                "toolchain",
                error,
                "Restore rust-toolchain.toml with a pinned Rust channel.",
            );
        }
    };

    match Command::new("rustc").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let installed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let expected = format!("rustc {configured}");
            if installed.starts_with(&expected) {
                CheckResult::pass("toolchain", format!("using {installed}"))
            } else {
                CheckResult::fail(
                    "toolchain",
                    format!("configured Rust {configured}, but found {installed}"),
                    format!(
                        "Install and select Rust {configured}: `rustup toolchain install {configured}`."
                    ),
                )
            }
        }
        Ok(output) => CheckResult::fail(
            "toolchain",
            format!("`rustc --version` exited with {}", output.status),
            format!("Install and select Rust {configured} with rustup."),
        ),
        Err(error) => CheckResult::fail(
            "toolchain",
            format!("could not execute rustc: {error}"),
            format!("Install and select Rust {configured} with rustup."),
        ),
    }
}

fn required_tooling_check() -> CheckResult {
    let mut missing = Vec::new();
    for requirement in REQUIRED_VERIFICATION_TOOLS {
        if !command_succeeds("cargo", requirement.arguments) {
            missing.push(requirement.name);
        }
    }

    let target_installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|target| target.trim() == "aarch64-apple-darwin")
        });
    if !target_installed {
        missing.push("aarch64-apple-darwin target");
    }

    let llvm_tools_installed = Command::new("rustup")
        .args(["component", "list", "--installed"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(is_llvm_tools_component)
        });
    if !llvm_tools_installed {
        missing.push("llvm-tools-preview component");
    }

    if missing.is_empty() {
        CheckResult::pass(
            "required verification tooling",
            "rustfmt, Clippy, llvm-tools-preview, nextest, cargo-deny, cargo-audit, cargo-llvm-cov, and the Apple Silicon target are installed",
        )
    } else {
        CheckResult::fail(
            "required verification tooling",
            format!("missing {}", missing.join(", ")),
            "Install the missing Cargo tools (`cargo install <tool> --locked`) and run `rustup component add rustfmt clippy llvm-tools-preview`; ensure `aarch64-apple-darwin` is installed with rustup.",
        )
    }
}

struct ToolRequirement {
    name: &'static str,
    arguments: &'static [&'static str],
}

const REQUIRED_VERIFICATION_TOOLS: &[ToolRequirement] = &[
    ToolRequirement {
        name: "rustfmt",
        arguments: &["fmt", "--version"],
    },
    ToolRequirement {
        name: "Clippy",
        arguments: &["clippy", "--version"],
    },
    ToolRequirement {
        name: "nextest",
        arguments: &["nextest", "--version"],
    },
    ToolRequirement {
        name: "cargo-deny",
        arguments: &["deny", "--version"],
    },
    ToolRequirement {
        name: "cargo-audit",
        arguments: &["audit", "--version"],
    },
    ToolRequirement {
        name: "cargo-llvm-cov",
        arguments: &["llvm-cov", "--version"],
    },
];

fn command_succeeds(program: &str, arguments: &[&str]) -> bool {
    Command::new(program)
        .args(arguments)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn is_llvm_tools_component(component: &str) -> bool {
    let component = component.trim();
    component == "llvm-tools-preview"
        || component == "llvm-tools"
        || component.starts_with("llvm-tools-preview-")
        || component.starts_with("llvm-tools-")
}

fn print_hardware_certification_status() {
    println!("\nHardware certification (not evaluated by doctor):");
    println!(
        "[PENDING] Attached-device verification: doctor does not inspect loopback, click, MIDI timing, or soak results."
    );
    println!(
        "          Action: run `cargo xtask loopback`, `click-test`, `midi-timing`, and `soak` with captured evidence on the designated Apple Silicon host."
    );
}

fn print_qualification_tooling() {
    println!("\nQualification tooling:");
    println!(
        "[REQUIRED] Static verification: cargo nextest, cargo-deny, cargo-audit, cargo-llvm-cov, rustfmt, Clippy, and the aarch64-apple-darwin target."
    );
    println!(
        "[REQUIRED FOR HARDWARE CERTIFICATION] Dedicated Apple Silicon host and a fixed 48 kHz stereo output route (for example, BlackHole or a reconfigurable device)."
    );
    let sdk = env::var_os("VST3_SDK_DIR").is_some_and(|path| Path::new(&path).is_dir());
    let cmake = command_succeeds("cmake", &["--version"]);
    let xcodebuild = command_succeeds("xcodebuild", &["-version"]);
    println!(
        "[OPTIONAL] VST3 SDK HostChecker: VST3_SDK_DIR={}, cmake={}, xcodebuild={}; required only when running SDK sample or HostChecker qualification.",
        availability(sdk),
        availability(cmake),
        availability(xcodebuild),
    );
    println!(
        "[OPTIONAL] A selected MIDI source and a licensed plug-in corpus are needed only for their corresponding hardware verification runs."
    );
}

const fn availability(available: bool) -> &'static str {
    if available { "available" } else { "not found" }
}

fn configured_toolchain(workspace_root: &Path) -> Result<String, String> {
    let path = workspace_root.join("rust-toolchain.toml");
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("channel").and_then(parse_toml_string))
        .ok_or_else(|| format!("could not find a quoted `channel` in {}", path.display()))
}

fn parse_toml_string(remainder: &str) -> Option<String> {
    let value = remainder.trim_start().strip_prefix('=')?.trim();
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    (!value.is_empty()).then(|| value.to_owned())
}

fn required_paths_check(workspace_root: &Path) -> CheckResult {
    let missing_directories = REQUIRED_DIRECTORIES
        .iter()
        .filter(|relative_path| !workspace_root.join(relative_path).is_dir())
        .map(|relative_path| format!("directory {relative_path}"))
        .collect::<Vec<_>>();
    let missing_files = REQUIRED_PATHS
        .iter()
        .filter(|relative_path| !workspace_root.join(relative_path).is_file())
        .map(|relative_path| format!("file {relative_path}"))
        .collect::<Vec<_>>();
    let missing = missing_directories
        .into_iter()
        .chain(missing_files)
        .collect::<Vec<_>>();

    if missing.is_empty() {
        CheckResult::pass(
            "required workspace layout",
            "all required directories and files are present",
        )
    } else {
        CheckResult::fail(
            "required workspace layout",
            format!("missing {}", missing.join(", ")),
            "Restore the missing directories or files.",
        )
    }
}

fn cargo_metadata(workspace_root: &Path) -> Result<PackageGraph, String> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--all-features",
        ])
        .current_dir(workspace_root)
        .output()
        .map_err(|error| format!("could not execute `cargo metadata`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("could not parse `cargo metadata` output: {error}"))?;
    graph_from_metadata(&metadata)
}

fn graph_from_metadata(metadata: &Value) -> Result<PackageGraph, String> {
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| "metadata contains no package list".to_owned())?;
    let names_by_id = packages
        .iter()
        .map(|package| {
            let id = package
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "metadata package has no id".to_owned())?;
            let name = package
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("metadata package {id} has no name"))?;
            Ok((id, name))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let nodes = metadata
        .pointer("/resolve/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "metadata contains no resolved dependency graph".to_owned())?;

    let mut graph = PackageGraph::default();
    for node in nodes {
        let package_id = node
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "metadata resolve node has no id".to_owned())?;
        let package_name = names_by_id
            .get(package_id)
            .ok_or_else(|| format!("resolve node references unknown package {package_id}"))?;
        graph.edges.entry((*package_name).to_owned()).or_default();

        let dependencies = node
            .get("deps")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("resolve node {package_name} has no dependencies"))?;
        for dependency in dependencies {
            let dependency_id = dependency
                .get("pkg")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("dependency in {package_name} has no package id"))?;
            let dependency_name = names_by_id.get(dependency_id).ok_or_else(|| {
                format!("dependency in {package_name} references unknown package {dependency_id}")
            })?;
            graph.insert_edge((*package_name).to_owned(), (*dependency_name).to_owned());
        }
    }

    Ok(graph)
}

fn dependency_boundary_check(graph: &PackageGraph) -> CheckResult {
    let prohibited_vst3_roots = ["superposition", "sp-engine"];
    let vst3_targets = ["sp-vst3", "vst3", "vst3-host"];
    let vst3_violations = prohibited_vst3_roots
        .iter()
        .flat_map(|root| {
            vst3_targets
                .iter()
                .filter(move |target| graph.contains_path(root, target))
                .map(move |target| format!("{root} -> {target}"))
        })
        .collect::<Vec<_>>();
    let helper_dependents = BTreeSet::from(["sp-plugin-worker", "sp-plugin-scanner"]);
    let host_dependents = BTreeSet::from(["sp-vst3"]);
    let sdk_dependents = BTreeSet::from(["vst3-host"]);
    let unexpected_dependents = [
        ("sp-vst3", &helper_dependents),
        ("vst3-host", &host_dependents),
        ("vst3", &sdk_dependents),
    ]
    .into_iter()
    .flat_map(|(crate_name, allowed)| {
        graph
            .direct_dependents_of(crate_name)
            .difference(allowed)
            .map(move |dependent| format!("{dependent} -> {crate_name}"))
            .collect::<Vec<_>>()
    })
    .collect::<Vec<_>>();

    let lower_layers = [
        "sp-model",
        "sp-protocol",
        "sp-shared-memory",
        "sp-engine",
        "sp-audio-io",
        "sp-vst3",
        "sp-supervisor",
        "sp-session",
        "sp-midi",
    ];
    let layer_violations = lower_layers
        .iter()
        .flat_map(|root| {
            ["superposition", "sp-ui"]
                .into_iter()
                .filter(move |target| graph.contains_path(root, target))
                .map(move |target| format!("{root} -> {target}"))
        })
        .collect::<Vec<_>>();

    if vst3_violations.is_empty() && unexpected_dependents.is_empty() && layer_violations.is_empty()
    {
        CheckResult::pass(
            "dependency boundary",
            "VST3 loading is helper-only and lower layers do not depend on the app or UI",
        )
    } else {
        let mut problems = Vec::new();
        if !vst3_violations.is_empty() {
            problems.push(format!(
                "VST3 loading is reachable through {}",
                vst3_violations.join(", ")
            ));
        }
        if !unexpected_dependents.is_empty() {
            problems.push(format!(
                "unexpected direct VST3 adapter/loading dependents: {}",
                unexpected_dependents.join(", ")
            ));
        }
        if !layer_violations.is_empty() {
            problems.push(format!(
                "invalid lower-layer dependency paths: {}",
                layer_violations.join(", ")
            ));
        }
        CheckResult::fail(
            "dependency boundary",
            problems.join("; "),
            "Keep VST3 loading in helpers and remove app/UI dependencies from lower-layer crates.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PackageGraph, REQUIRED_VERIFICATION_TOOLS, availability, dependency_boundary_check,
        graph_from_metadata, is_llvm_tools_component, parse_toml_string,
    };
    use serde_json::json;

    #[test]
    fn finds_transitive_dependency_paths() {
        let mut graph = PackageGraph::default();
        graph.insert_edge("superposition", "sp-engine");
        graph.insert_edge("sp-engine", "sp-vst3");

        assert!(graph.contains_path("superposition", "sp-vst3"));
        assert!(!graph.contains_path("sp-vst3", "superposition"));
    }

    #[test]
    fn rejects_lower_layer_ui_dependencies() {
        let mut graph = PackageGraph::default();
        graph.insert_edge("sp-engine", "sp-ui");

        let result = dependency_boundary_check(&graph);
        assert!(!result.passed);
        assert!(result.detail.contains("sp-engine -> sp-ui"));
    }

    #[test]
    fn rejects_engine_dependency_on_vst3_host() {
        let mut graph = PackageGraph::default();
        graph.insert_edge("sp-engine", "vst3-host");

        let result = dependency_boundary_check(&graph);
        assert!(!result.passed);
        assert!(result.detail.contains("sp-engine -> vst3-host"));
    }

    #[test]
    fn parses_resolved_metadata_edges() {
        let metadata = json!({
            "packages": [
                {"id": "app-id", "name": "superposition"},
                {"id": "engine-id", "name": "sp-engine"},
                {"id": "vst-id", "name": "sp-vst3"}
            ],
            "resolve": {
                "nodes": [
                    {"id": "app-id", "deps": [{"pkg": "engine-id"}]},
                    {"id": "engine-id", "deps": [{"pkg": "vst-id"}]},
                    {"id": "vst-id", "deps": []}
                ]
            }
        });

        let graph = graph_from_metadata(&metadata).expect("metadata graph should parse");
        assert!(graph.contains_path("superposition", "sp-vst3"));
    }

    #[test]
    fn parses_quoted_toolchain_channel() {
        assert_eq!(
            parse_toml_string(" = \"1.95.0\""),
            Some("1.95.0".to_owned())
        );
        assert_eq!(parse_toml_string(" = 1.95"), None);
    }

    #[test]
    fn requires_the_full_static_verification_toolchain() {
        let names = REQUIRED_VERIFICATION_TOOLS
            .iter()
            .map(|requirement| requirement.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "rustfmt",
                "Clippy",
                "nextest",
                "cargo-deny",
                "cargo-audit",
                "cargo-llvm-cov",
            ]
        );
    }

    #[test]
    fn recognizes_legacy_and_target_qualified_llvm_tool_components() {
        assert!(is_llvm_tools_component("llvm-tools-preview"));
        assert!(is_llvm_tools_component("llvm-tools-aarch64-apple-darwin"));
        assert!(!is_llvm_tools_component("rustfmt-aarch64-apple-darwin"));
    }

    #[test]
    fn qualification_tool_availability_is_explicit() {
        assert_eq!(availability(true), "available");
        assert_eq!(availability(false), "not found");
    }
}
