//! Parent-supervised isolated VST3 scanning.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use sp_supervisor::{
    CapturedHelperOutput, HelperKind, HelperLaunch, ProcessSupervisor, TimedHelperResult,
};

use crate::outcome::{CommandError, CommandOutcome};

/// Default disposable-scanner timeout from the product plan.
pub(crate) const DEFAULT_SCAN_TIMEOUT: Duration = Duration::from_secs(10);

/// Machine-readable isolated scan outcome for host-checker / corpus tooling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IsolatedScanReport {
    pub bundle: PathBuf,
    pub outcome: &'static str,
    pub descriptors: Vec<IsolatedDescriptor>,
    pub exit_code: Option<i32>,
    pub detail: Option<String>,
    pub raw_stdout: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IsolatedDescriptor {
    pub class_id: String,
    pub name: String,
    pub vendor: String,
}

/// Launches `sp-plugin-scanner --sdk-enumerate` under [`ProcessSupervisor`] with a timeout.
pub(crate) fn scan_bundle_isolated(
    scanner: &Path,
    bundle: &Path,
    timeout: Duration,
) -> Result<IsolatedScanReport, CommandError> {
    let mut supervisor = ProcessSupervisor::new();
    let launch = HelperLaunch {
        kind: HelperKind::PluginScanner,
        executable: scanner.to_path_buf(),
        arguments: vec![
            "--bundle".to_owned(),
            bundle.display().to_string(),
            "--json".to_owned(),
            "--sdk-enumerate".to_owned(),
        ],
    };
    let captured = supervisor
        .launch_and_wait_capturing(&launch, timeout)
        .map_err(|error| {
            CommandError::Infrastructure(format!("could not launch isolated scanner: {error}"))
        })?;
    Ok(map_captured_scan(bundle, &captured))
}

fn map_captured_scan(bundle: &Path, captured: &CapturedHelperOutput) -> IsolatedScanReport {
    let stdout = String::from_utf8_lossy(&captured.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&captured.stderr).trim().to_owned();
    match &captured.result {
        TimedHelperResult::TimedOut { .. } => IsolatedScanReport {
            bundle: bundle.to_path_buf(),
            outcome: "timed_out",
            descriptors: Vec::new(),
            exit_code: None,
            detail: Some(format!(
                "scanner exceeded {}; stderr={}",
                format_duration(DEFAULT_SCAN_TIMEOUT),
                truncate(&stderr, 240)
            )),
            raw_stdout: stdout,
        },
        TimedHelperResult::Exited { code, .. } => {
            let code = *code;
            if let Some(parsed) = parse_child_report(&stdout) {
                let outcome = if code == 0 {
                    parsed.outcome
                } else if parsed.outcome == "supported" {
                    // Unexpected non-zero with a supported report → treat as crash/fault.
                    "crashed"
                } else {
                    parsed.outcome
                };
                IsolatedScanReport {
                    bundle: bundle.to_path_buf(),
                    outcome,
                    descriptors: parsed.descriptors,
                    exit_code: Some(code),
                    detail: parsed.detail.or_else(|| {
                        if code == 0 {
                            None
                        } else if stderr.is_empty() {
                            Some(format!("scanner exited with code {code}"))
                        } else {
                            Some(truncate(&stderr, 240))
                        }
                    }),
                    raw_stdout: stdout,
                }
            } else if code == 0 {
                IsolatedScanReport {
                    bundle: bundle.to_path_buf(),
                    outcome: "crashed",
                    descriptors: Vec::new(),
                    exit_code: Some(code),
                    detail: Some("scanner exited 0 without parseable JSON".to_owned()),
                    raw_stdout: stdout,
                }
            } else {
                IsolatedScanReport {
                    bundle: bundle.to_path_buf(),
                    outcome: "crashed",
                    descriptors: Vec::new(),
                    exit_code: Some(code),
                    detail: Some(if stderr.is_empty() {
                        format!("scanner exited with code {code} and no JSON report")
                    } else {
                        truncate(&stderr, 240)
                    }),
                    raw_stdout: stdout,
                }
            }
        }
    }
}

struct ParsedChildReport {
    outcome: &'static str,
    descriptors: Vec<IsolatedDescriptor>,
    detail: Option<String>,
}

fn parse_child_report(stdout: &str) -> Option<ParsedChildReport> {
    let value: Value = serde_json::from_str(stdout).ok()?;
    let outcome = match value.get("outcome").and_then(Value::as_str)? {
        "supported" => "supported",
        "unsupported_architecture" => "unsupported_architecture",
        "invalid_bundle" => "invalid_bundle",
        "timed_out" => "timed_out",
        "crashed" => "crashed",
        "sdk_error" => "sdk_error",
        _ => return None,
    };
    let descriptors = value
        .get("descriptors")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some(IsolatedDescriptor {
                        class_id: entry.get("class_id")?.as_str()?.to_owned(),
                        name: entry.get("name")?.as_str()?.to_owned(),
                        vendor: entry.get("vendor")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(ParsedChildReport {
        outcome,
        descriptors,
        detail,
    })
}

fn format_duration(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let shortened: String = text.chars().take(max_chars).collect();
        format!("{shortened}…")
    }
}

/// Converts an isolated scan into the shared xtask command outcome helper.
#[allow(dead_code)]
pub(crate) fn isolated_scan_passed(report: &IsolatedScanReport) -> CommandOutcome {
    if report.outcome == "supported" && !report.descriptors.is_empty() {
        CommandOutcome::passed()
    } else {
        CommandOutcome::acceptance_failure()
    }
}

#[cfg(test)]
mod tests {
    use super::map_captured_scan;
    use sp_supervisor::{CapturedHelperOutput, TimedHelperResult};
    use std::path::Path;

    #[test]
    fn maps_timeout_to_timed_out() {
        let report = map_captured_scan(
            Path::new("Example.vst3"),
            &CapturedHelperOutput {
                result: TimedHelperResult::TimedOut { process_id: 1 },
                stdout: Vec::new(),
                stderr: b"hung".to_vec(),
            },
        );
        assert_eq!(report.outcome, "timed_out");
    }

    #[test]
    fn maps_parseable_supported_report() {
        let stdout = br#"{"bundle":"Example.vst3","architectures":"arm64","supported_on_apple_silicon":true,"outcome":"supported","descriptors":[{"class_id":"ABC","name":"Again","vendor":"Steinberg"}],"sdk_enumerate":true,"detail":null}"#;
        let report = map_captured_scan(
            Path::new("Example.vst3"),
            &CapturedHelperOutput {
                result: TimedHelperResult::Exited {
                    process_id: 1,
                    code: 0,
                },
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
            },
        );
        assert_eq!(report.outcome, "supported");
        assert_eq!(report.descriptors.len(), 1);
        assert_eq!(report.descriptors[0].name, "Again");
    }

    #[test]
    fn maps_nonzero_without_json_to_crashed() {
        let report = map_captured_scan(
            Path::new("Example.vst3"),
            &CapturedHelperOutput {
                result: TimedHelperResult::Exited {
                    process_id: 1,
                    code: 9,
                },
                stdout: Vec::new(),
                stderr: b"fatal".to_vec(),
            },
        );
        assert_eq!(report.outcome, "crashed");
    }
}
