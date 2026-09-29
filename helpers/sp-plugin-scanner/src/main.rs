//! Disposable VST3 bundle scanner.
//!
//! Bundle layout and Mach-O architecture inspection always happen before any factory code is
//! eligible to load. The parent launches this helper once per bundle and applies its deadline;
//! all machine-readable data is serialized as the SDK-free scan-metadata schema.

use std::{env, fs, path::PathBuf, process};

use serde::Serialize;
use sp_model::{PluginArchitecture, PluginScanMetadata, PluginScanOutcome};
use sp_vst3::{Vst3Architecture, Vst3BundleInfo, Vst3BundlePath, inspect_bundle_layout};

#[cfg(feature = "sdk")]
use sp_vst3::{
    adapter::{ProcessingFormat, Vst3ClassSelection},
    sdk::{HostSdkRackFactory, SDK_MAX_FRAMES},
};

/// Request passed to the scanner before it inspects a VST3 bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRequest {
    /// Bundle path that must be inspected outside the application process.
    pub bundle: Vst3BundlePath,
}

/// Machine-readable result emitted by the disposable scanner helper.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    bundle: PathBuf,
    #[serde(flatten)]
    metadata: PluginScanMetadata,
    sdk_enumerate: bool,
}

struct Args {
    bundle: PathBuf,
    json: bool,
    sdk_enumerate: bool,
    report_path: Option<PathBuf>,
}

fn main() {
    let args = match parse_args(env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!(
                "{message}\nusage: sp-plugin-scanner --bundle <path> [--json] [--sdk-enumerate] [--report-path <path>]"
            );
            process::exit(2);
        }
    };
    let report = match inspect_bundle_layout(&args.bundle) {
        Ok(info) => report_for_info(info, args.sdk_enumerate),
        Err(_) => invalid_report(args.bundle),
    };

    if let Some(path) = &args.report_path {
        let bytes = serde_json::to_vec(&report).expect("ScanReport is serializable");
        if let Err(error) = fs::write(path, bytes) {
            eprintln!(
                "could not write scanner report to {}: {error}",
                path.display()
            );
            process::exit(2);
        }
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("ScanReport is serializable")
        );
    } else {
        println!("bundle: {}", report.bundle.display());
        println!(
            "architecture: {}",
            architecture_name(report.metadata.architecture)
        );
        println!("outcome: {}", outcome_name(report.metadata.outcome));
        println!("classes: {}", report.metadata.classes.len());
        println!("sdk_enumerate: {}", report.sdk_enumerate);
        if let Some(detail) = &report.metadata.detail {
            println!("detail: {detail}");
        }
    }
    if report.metadata.outcome != PluginScanOutcome::Supported {
        process::exit(1);
    }
}

fn parse_args(arguments: impl Iterator<Item = String>) -> Result<Args, &'static str> {
    let mut bundle = None;
    let mut json = false;
    let mut sdk_enumerate = false;
    let mut report_path = None;
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--bundle" if bundle.is_none() => bundle = arguments.next().map(PathBuf::from),
            "--json" => json = true,
            "--sdk-enumerate" => sdk_enumerate = true,
            "--report-path" if report_path.is_none() => {
                report_path = arguments.next().map(PathBuf::from);
                if report_path.is_none() {
                    return Err("--report-path requires a path");
                }
            }
            _ => return Err("invalid arguments"),
        }
    }
    bundle
        .map(|bundle| Args {
            bundle,
            json,
            sdk_enumerate,
            report_path,
        })
        .ok_or("--bundle is required")
}

fn report_for_info(info: Vst3BundleInfo, sdk_enumerate: bool) -> ScanReport {
    let architecture = architecture_from(info.architectures);
    if !info.supported_on_apple_silicon {
        return report(
            info.path,
            metadata_with_detail(
                architecture,
                PluginScanOutcome::UnsupportedArchitecture,
                "bundle has no native Apple Silicon code",
            ),
            false,
        );
    }

    if !sdk_enumerate {
        return report(
            info.path,
            metadata_with_detail(
                architecture,
                PluginScanOutcome::InvalidReport,
                "layout probe completed; full class metadata requires --sdk-enumerate",
            ),
            false,
        );
    }

    scan_with_sdk(info, architecture)
}

/// Enumerates every audio-module class, then obtains complete helper-safe metadata for each one.
///
/// The SDK adapter remains confined to this disposable process. It selects classes while inactive
/// and never starts audio processing or opens a native editor during scan.
#[cfg(feature = "sdk")]
fn scan_with_sdk(info: Vst3BundleInfo, architecture: PluginArchitecture) -> ScanReport {
    let bundle = Vst3BundlePath::new(info.path.clone());
    let factory = HostSdkRackFactory;
    let format = match ProcessingFormat::new(48_000.0, SDK_MAX_FRAMES) {
        Ok(format) => format,
        Err(error) => {
            return report(
                info.path,
                metadata_with_detail(architecture, PluginScanOutcome::SdkError, error.to_string()),
                true,
            );
        }
    };
    let descriptors = match factory.enumerate_classes(&bundle) {
        Ok(descriptors) => descriptors,
        Err(error) => {
            return report(
                info.path,
                metadata_with_detail(architecture, PluginScanOutcome::SdkError, error.to_string()),
                true,
            );
        }
    };
    if descriptors.is_empty() {
        return report(
            info.path,
            metadata_with_detail(
                architecture,
                PluginScanOutcome::InvalidReport,
                "bundle exposes no audio-module classes",
            ),
            true,
        );
    }

    let mut metadata = PluginScanMetadata::new(architecture, PluginScanOutcome::Supported);
    for descriptor in descriptors {
        let class_id = descriptor.class_id;
        let selection = Vst3ClassSelection::new(bundle.clone(), class_id.clone());
        match factory.scan_class_metadata(&selection, format) {
            Ok(class) => metadata.classes.push(class),
            Err(error) => {
                return report(
                    info.path,
                    metadata_with_detail(
                        architecture,
                        PluginScanOutcome::SdkError,
                        format!("could not inspect VST3 class {class_id}: {error}"),
                    ),
                    true,
                );
            }
        }
    }
    report(info.path, metadata, true)
}

#[cfg(not(feature = "sdk"))]
fn scan_with_sdk(info: Vst3BundleInfo, architecture: PluginArchitecture) -> ScanReport {
    report(
        info.path,
        metadata_with_detail(
            architecture,
            PluginScanOutcome::SdkError,
            "scanner helper was built without the sdk feature",
        ),
        true,
    )
}

fn invalid_report(bundle: PathBuf) -> ScanReport {
    report(
        bundle,
        metadata_with_detail(
            PluginArchitecture::Unknown,
            PluginScanOutcome::InvalidBundle,
            "path is not an inspectable VST3 bundle",
        ),
        false,
    )
}

fn report(bundle: PathBuf, metadata: PluginScanMetadata, sdk_enumerate: bool) -> ScanReport {
    ScanReport {
        bundle,
        metadata,
        sdk_enumerate,
    }
}

fn metadata_with_detail(
    architecture: PluginArchitecture,
    outcome: PluginScanOutcome,
    detail: impl Into<String>,
) -> PluginScanMetadata {
    let mut metadata = PluginScanMetadata::new(architecture, outcome);
    metadata.detail = Some(detail.into());
    metadata
}

const fn architecture_from(architecture: Vst3Architecture) -> PluginArchitecture {
    match architecture {
        Vst3Architecture::Arm64 => PluginArchitecture::Arm64,
        Vst3Architecture::X86_64 => PluginArchitecture::X86_64,
        Vst3Architecture::Universal => PluginArchitecture::Universal,
        Vst3Architecture::Unknown => PluginArchitecture::Unknown,
    }
}

const fn architecture_name(architecture: PluginArchitecture) -> &'static str {
    match architecture {
        PluginArchitecture::Arm64 => "arm64",
        PluginArchitecture::X86_64 => "x86_64",
        PluginArchitecture::Universal => "universal",
        PluginArchitecture::Unknown => "unknown",
    }
}

const fn outcome_name(outcome: PluginScanOutcome) -> &'static str {
    match outcome {
        PluginScanOutcome::Supported => "supported",
        PluginScanOutcome::UnsupportedArchitecture => "unsupported_architecture",
        PluginScanOutcome::InvalidBundle => "invalid_bundle",
        PluginScanOutcome::TimedOut => "timed_out",
        PluginScanOutcome::Crashed => "crashed",
        PluginScanOutcome::SdkError => "sdk_error",
        PluginScanOutcome::InvalidReport => "invalid_report",
    }
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn parses_required_bundle_json_sdk_flag_and_report_path() {
        let args = parse_args(
            [
                "--bundle",
                "Example.vst3",
                "--json",
                "--sdk-enumerate",
                "--report-path",
                "/tmp/report.json",
            ]
            .map(str::to_owned)
            .into_iter(),
        )
        .expect("arguments should parse");

        assert_eq!(args.bundle.to_string_lossy(), "Example.vst3");
        assert!(args.json);
        assert!(args.sdk_enumerate);
        assert_eq!(
            args.report_path.as_deref(),
            Some(std::path::Path::new("/tmp/report.json"))
        );
    }

    #[test]
    fn rejects_report_path_without_a_value() {
        assert!(
            parse_args(
                ["--bundle", "Example.vst3", "--report-path"]
                    .map(str::to_owned)
                    .into_iter(),
            )
            .is_err()
        );
    }
}
