//! Reproducible Phase 0 static-foundation evidence.
//!
//! The report deliberately executes only static checks. Device-attached timing and plug-in
//! qualification require later, controlled hardware runs and are recorded as pending here.

use std::{env, fmt::Write as _, fs, path::Path, process::Command};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{cargo_metadata, dependency_boundary_check};

const REPORT_VERSION: u32 = 2;
const OUTPUT_DIRECTORY: &str = "target/phase0";

/// Writes the Phase 0 static-foundation evidence to its deterministic target path.
///
/// # Errors
///
/// Returns an error after writing the report when an argument is unsupported or one of the
/// required static checks fails. The report retains the observed command outcomes in either
/// case.
pub(crate) fn run_foundation_report(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<(), String> {
    if !arguments.is_empty() {
        return Err(format!(
            "phase0-report does not accept arguments\n\n{}",
            foundation_report_usage()
        ));
    }

    let output_directory = workspace_root.join(OUTPUT_DIRECTORY);
    let log_directory = output_directory.join("logs");
    fs::create_dir_all(&log_directory).map_err(|error| {
        format!(
            "could not create Phase 0 evidence directory {}: {error}",
            output_directory.display()
        )
    })?;

    let commands = vec![
        StaticCommand::cargo("doctor", &["xtask", "doctor"]),
        StaticCommand::cargo("format", &["fmt", "--all", "--", "--check"]),
        StaticCommand::cargo(
            "clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ],
        ),
        StaticCommand::cargo(
            "nextest",
            &["nextest", "run", "--workspace", "--all-features"],
        ),
        StaticCommand::cargo("deny", &["deny", "check"]),
        StaticCommand::cargo("audit", &["audit"]),
        coverage_command(),
    ]
    .into_iter()
    .map(|command| run_static_command(workspace_root, &log_directory, command))
    .collect::<Result<Vec<_>, _>>()?;

    let dependency_boundary = dependency_boundary_evidence(workspace_root);
    let implementation_readiness =
        commands.iter().all(|command| command.passed) && dependency_boundary.passed;
    let report = FoundationReport {
        report_version: REPORT_VERSION,
        source: source_evidence(workspace_root)?,
        environment: EnvironmentEvidence {
            operating_system: env::consts::OS.to_owned(),
            architecture: env::consts::ARCH.to_owned(),
            macos_version: command_value(workspace_root, "sw_vers", &["-productVersion"]),
            macos_build: command_value(workspace_root, "sw_vers", &["-buildVersion"]),
            hardware_model: command_value(workspace_root, "sysctl", &["-n", "hw.model"]),
            hardware_memory_bytes: command_value(
                workspace_root,
                "sysctl",
                &["-n", "hw.memsize"],
            ),
            rust_version: command_value(workspace_root, "rustc", &["--version"]),
            cargo_version: command_value(workspace_root, "cargo", &["--version"]),
            xcode_version: command_value(workspace_root, "xcodebuild", &["-version"]),
            cmake_version: command_value(workspace_root, "cmake", &["--version"]),
            verification_tool_versions: VerificationToolVersions {
                nextest: command_value(workspace_root, "cargo", &["nextest", "--version"]),
                deny: command_value(workspace_root, "cargo", &["deny", "--version"]),
                audit: command_value(workspace_root, "cargo", &["audit", "--version"]),
                llvm_cov: command_value(workspace_root, "cargo", &["llvm-cov", "--version"]),
            },
            vst3_sdk_dir: env::var("VST3_SDK_DIR")
                .ok()
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "not configured".to_owned()),
        },
        static_checks: commands,
        dependency_boundary,
        implementation_readiness,
        hardware_certification: HardwareCertification {
            evaluated: false,
            phase1_hard_gate_certified: false,
            reason: "Phase 0 static verification does not run device-attached timing, plug-in, MIDI, loopback, fault-containment, or soak qualifications.".to_owned(),
        },
    };
    write_report(&output_directory, &report)?;

    println!(
        "PHASE0_FOUNDATION: implementation_readiness={}, hardware_certification=pending, artifacts={}",
        report.implementation_readiness,
        output_directory.display()
    );

    if report.implementation_readiness {
        Ok(())
    } else {
        Err(format!(
            "Phase 0 static foundation check failed; inspect {}",
            output_directory.join("foundation-report.json").display()
        ))
    }
}

/// Returns the command synopsis for the Phase 0 evidence writer.
pub(crate) const fn foundation_report_usage() -> &'static str {
    "phase0-report"
}

#[derive(Clone, Copy)]
struct StaticCommand {
    name: &'static str,
    program: &'static str,
    arguments: &'static [&'static str],
}

impl StaticCommand {
    const fn cargo(name: &'static str, arguments: &'static [&'static str]) -> Self {
        Self {
            name,
            program: "cargo",
            arguments,
        }
    }
}

fn coverage_command() -> StaticCommand {
    StaticCommand::cargo(
        "coverage",
        &["llvm-cov", "nextest", "--workspace", "--all-features"],
    )
}

#[derive(Serialize)]
struct FoundationReport {
    report_version: u32,
    source: SourceEvidence,
    environment: EnvironmentEvidence,
    static_checks: Vec<CommandEvidence>,
    dependency_boundary: DependencyBoundaryEvidence,
    implementation_readiness: bool,
    hardware_certification: HardwareCertification,
}

#[derive(Serialize)]
struct SourceEvidence {
    revision: String,
    state: String,
    manifest: SourceManifest,
}

#[derive(Serialize)]
struct SourceManifest {
    algorithm: &'static str,
    tracked_diff_sha256: String,
    untracked_files: Vec<SourceFileDigest>,
    manifest_sha256: String,
    excluded_evidence_paths: &'static [&'static str],
}

#[derive(Serialize)]
struct SourceFileDigest {
    path: String,
    sha256: String,
}

#[derive(Serialize)]
struct EnvironmentEvidence {
    operating_system: String,
    architecture: String,
    macos_version: String,
    macos_build: String,
    hardware_model: String,
    hardware_memory_bytes: String,
    rust_version: String,
    cargo_version: String,
    xcode_version: String,
    cmake_version: String,
    verification_tool_versions: VerificationToolVersions,
    vst3_sdk_dir: String,
}

#[derive(Serialize)]
struct VerificationToolVersions {
    nextest: String,
    deny: String,
    audit: String,
    llvm_cov: String,
}

#[derive(Serialize)]
struct CommandEvidence {
    name: &'static str,
    command: String,
    passed: bool,
    exit_code: Option<i32>,
    stdout_log: String,
    stdout_sha256: String,
    stderr_log: String,
    stderr_sha256: String,
}

#[derive(Serialize)]
struct DependencyBoundaryEvidence {
    passed: bool,
    detail: String,
}

#[derive(Serialize)]
struct HardwareCertification {
    evaluated: bool,
    phase1_hard_gate_certified: bool,
    reason: String,
}

fn run_static_command(
    workspace_root: &Path,
    log_directory: &Path,
    command: StaticCommand,
) -> Result<CommandEvidence, String> {
    let command_line = format!("{} {}", command.program, command.arguments.join(" "));
    let output = Command::new(command.program)
        .args(command.arguments)
        .current_dir(workspace_root)
        .output();
    let (passed, exit_code, stdout, stderr) = match output {
        Ok(output) => (
            output.status.success(),
            output.status.code(),
            output.stdout,
            output.stderr,
        ),
        Err(error) => (false, None, Vec::new(), error.to_string().into_bytes()),
    };
    let stdout_log = format!("{}.stdout.log", command.name);
    let stderr_log = format!("{}.stderr.log", command.name);
    write_log(&log_directory.join(&stdout_log), &stdout)?;
    write_log(&log_directory.join(&stderr_log), &stderr)?;

    Ok(CommandEvidence {
        name: command.name,
        command: command_line,
        passed,
        exit_code,
        stdout_log: format!("logs/{stdout_log}"),
        stdout_sha256: sha256_hex(&stdout),
        stderr_log: format!("logs/{stderr_log}"),
        stderr_sha256: sha256_hex(&stderr),
    })
}

fn dependency_boundary_evidence(workspace_root: &Path) -> DependencyBoundaryEvidence {
    match cargo_metadata(workspace_root) {
        Ok(graph) => {
            let result = dependency_boundary_check(&graph);
            DependencyBoundaryEvidence {
                passed: result.passed,
                detail: result.detail,
            }
        }
        Err(error) => DependencyBoundaryEvidence {
            passed: false,
            detail: format!("Cargo metadata could not be read: {error}"),
        },
    }
}

fn write_log(path: &Path, contents: &[u8]) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn write_report(output_directory: &Path, report: &FoundationReport) -> Result<(), String> {
    let report_path = output_directory.join("foundation-report.json");
    let temporary_path = output_directory.join("foundation-report.json.tmp");
    let encoded = serde_json::to_vec_pretty(report)
        .map_err(|error| format!("could not encode Phase 0 report: {error}"))?;
    fs::write(&temporary_path, encoded).map_err(|error| {
        format!(
            "could not write temporary Phase 0 report {}: {error}",
            temporary_path.display()
        )
    })?;
    fs::rename(&temporary_path, &report_path).map_err(|error| {
        format!(
            "could not publish Phase 0 report {}: {error}",
            report_path.display()
        )
    })
}

fn command_value(workspace_root: &Path, command: &str, arguments: &[&str]) -> String {
    Command::new(command)
        .args(arguments)
        .current_dir(workspace_root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn source_evidence(workspace_root: &Path) -> Result<SourceEvidence, String> {
    Ok(SourceEvidence {
        revision: command_value(workspace_root, "git", &["rev-parse", "HEAD"]),
        state: source_state(workspace_root),
        manifest: source_manifest(workspace_root)?,
    })
}

fn source_manifest(workspace_root: &Path) -> Result<SourceManifest, String> {
    let tracked_diff = command_bytes(
        workspace_root,
        "git",
        &["diff", "--binary", "--no-ext-diff", "HEAD", "--"],
    )?;
    let untracked_paths = command_bytes(
        workspace_root,
        "git",
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let mut untracked_files = Vec::new();
    for path_bytes in untracked_paths.split(|byte| *byte == 0) {
        if path_bytes.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(path_bytes)
            .map_err(|error| format!("untracked path is not UTF-8: {error}"))?;
        if is_qualification_evidence_path(path) {
            continue;
        }
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(format!("untracked path escapes the workspace: {path}"));
        }
        let contents = fs::read(workspace_root.join(relative))
            .map_err(|error| format!("could not hash untracked source file {path}: {error}"))?;
        untracked_files.push(SourceFileDigest {
            path: path.to_owned(),
            sha256: sha256_hex(&contents),
        });
    }
    untracked_files.sort_by(|left, right| left.path.cmp(&right.path));

    let tracked_diff_sha256 = sha256_hex(&tracked_diff);
    let mut manifest = Sha256::new();
    manifest.update(b"superposition-phase0-source-manifest-v1\0");
    manifest.update(tracked_diff_sha256.as_bytes());
    for file in &untracked_files {
        manifest.update(file.path.as_bytes());
        manifest.update(b"\0");
        manifest.update(file.sha256.as_bytes());
        manifest.update(b"\0");
    }

    Ok(SourceManifest {
        algorithm: "sha256",
        tracked_diff_sha256,
        untracked_files,
        manifest_sha256: sha256_hex(&manifest.finalize()),
        excluded_evidence_paths: &["docs/qualification/**"],
    })
}

fn is_qualification_evidence_path(path: &str) -> bool {
    path.starts_with("docs/qualification/")
}

fn command_bytes(
    workspace_root: &Path,
    command: &str,
    arguments: &[&str],
) -> Result<Vec<u8>, String> {
    let output = Command::new(command)
        .args(arguments)
        .current_dir(workspace_root)
        .output()
        .map_err(|error| format!("could not execute `{command}`: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "`{command} {}` failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn source_state(workspace_root: &Path) -> String {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace_root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unavailable".to_owned(),
            |output| {
                if output.stdout.is_empty() {
                    "clean".to_owned()
                } else {
                    "dirty".to_owned()
                }
            },
        )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{foundation_report_usage, sha256_hex};

    #[test]
    fn foundation_report_uses_a_stable_command_name() {
        assert_eq!(foundation_report_usage(), "phase0-report");
    }

    #[test]
    fn evidence_hashes_retain_empty_and_nonempty_log_distinctions() {
        assert_ne!(sha256_hex(b""), sha256_hex(b"static check output"));
    }
}
