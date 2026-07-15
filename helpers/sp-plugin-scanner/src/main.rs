//! Isolated VST3 bundle scanner.
//!
//! Default path probes bundle layout without loading code. With `--sdk-enumerate`
//! (enabled by the default `sdk` feature) the helper also loads factory metadata
//! inside this disposable process so the parent can attribute timeout/crash outcomes.

use serde::Serialize;
use sp_vst3::{
    ScanOutcome, Vst3Architecture, Vst3BundleInfo, Vst3BundlePath, inspect_bundle_layout,
};
use std::env;
use std::path::PathBuf;
use std::process;

#[cfg(feature = "sdk")]
use sp_vst3::sdk::{HostSdkFactory, SdkPluginFactory};

/// Request passed to the scanner before it inspects a VST3 bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRequest {
    /// Bundle path that must be inspected outside the application process.
    pub bundle: Vst3BundlePath,
}

/// Machine-readable result emitted by this scanner phase.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    bundle: PathBuf,
    architectures: &'static str,
    supported_on_apple_silicon: bool,
    outcome: &'static str,
    descriptors: Vec<DescriptorReport>,
    sdk_enumerate: bool,
    detail: Option<String>,
}

/// Descriptor shape populated by the isolated SDK scan.
#[derive(Debug, Serialize)]
struct DescriptorReport {
    class_id: String,
    name: String,
    vendor: String,
}

struct Args {
    bundle: PathBuf,
    json: bool,
    sdk_enumerate: bool,
}

fn main() {
    let args = match parse_args(env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!(
                "{message}\nusage: sp-plugin-scanner --bundle <path> [--json] [--sdk-enumerate]"
            );
            process::exit(2);
        }
    };
    let report = match inspect_bundle_layout(&args.bundle) {
        Ok(info) => report_for_info(info, args.sdk_enumerate),
        Err(_) => invalid_report(args.bundle),
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("ScanReport is serializable")
        );
    } else {
        println!("bundle: {}", report.bundle.display());
        println!("architectures: {}", report.architectures);
        println!(
            "supported_on_apple_silicon: {}",
            report.supported_on_apple_silicon
        );
        println!("outcome: {}", report.outcome);
        println!("descriptors: {}", report.descriptors.len());
        println!("sdk_enumerate: {}", report.sdk_enumerate);
        if let Some(detail) = &report.detail {
            println!("detail: {detail}");
        }
        for descriptor in &report.descriptors {
            println!(
                "  - {} ({}) [{}]",
                descriptor.name, descriptor.vendor, descriptor.class_id
            );
        }
    }
    if matches!(
        report.outcome,
        "invalid_bundle" | "unsupported_architecture"
    ) {
        process::exit(1);
    }
    if report.outcome == "sdk_error" {
        process::exit(3);
    }
}

fn parse_args(arguments: impl Iterator<Item = String>) -> Result<Args, &'static str> {
    let mut bundle = None;
    let mut json = false;
    let mut sdk_enumerate = false;
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--bundle" if bundle.is_none() => bundle = arguments.next().map(PathBuf::from),
            "--json" => json = true,
            "--sdk-enumerate" => sdk_enumerate = true,
            _ => return Err("invalid arguments"),
        }
    }
    bundle
        .map(|bundle| Args {
            bundle,
            json,
            sdk_enumerate,
        })
        .ok_or("--bundle is required")
}

fn report_for_info(info: Vst3BundleInfo, sdk_enumerate: bool) -> ScanReport {
    if !info.supported_on_apple_silicon {
        return ScanReport {
            bundle: info.path,
            architectures: info.architectures.as_str(),
            supported_on_apple_silicon: false,
            outcome: outcome_name(ScanOutcome::UnsupportedArchitecture),
            descriptors: Vec::new(),
            sdk_enumerate: false,
            detail: Some("bundle has no native Apple Silicon code".to_owned()),
        };
    }

    if !sdk_enumerate {
        return ScanReport {
            bundle: info.path,
            architectures: info.architectures.as_str(),
            supported_on_apple_silicon: true,
            outcome: outcome_name(ScanOutcome::Supported),
            descriptors: Vec::new(),
            sdk_enumerate: false,
            detail: Some("layout probe only; pass --sdk-enumerate for factory metadata".to_owned()),
        };
    }

    enumerate_with_sdk(info)
}

#[cfg(feature = "sdk")]
fn enumerate_with_sdk(info: Vst3BundleInfo) -> ScanReport {
    let bundle = Vst3BundlePath::new(info.path.clone());
    match HostSdkFactory.enumerate(&bundle) {
        Ok(descriptors) => ScanReport {
            bundle: info.path,
            architectures: info.architectures.as_str(),
            supported_on_apple_silicon: true,
            outcome: outcome_name(ScanOutcome::Supported),
            descriptors: descriptors
                .into_iter()
                .map(|descriptor| DescriptorReport {
                    class_id: descriptor.class_id,
                    name: descriptor.name,
                    vendor: descriptor.vendor,
                })
                .collect(),
            sdk_enumerate: true,
            detail: None,
        },
        Err(error) => ScanReport {
            bundle: info.path,
            architectures: info.architectures.as_str(),
            supported_on_apple_silicon: true,
            outcome: "sdk_error",
            descriptors: Vec::new(),
            sdk_enumerate: true,
            detail: Some(error.to_string()),
        },
    }
}

#[cfg(not(feature = "sdk"))]
fn enumerate_with_sdk(info: Vst3BundleInfo) -> ScanReport {
    ScanReport {
        bundle: info.path,
        architectures: info.architectures.as_str(),
        supported_on_apple_silicon: true,
        outcome: "sdk_error",
        descriptors: Vec::new(),
        sdk_enumerate: true,
        detail: Some("scanner built without the sdk feature".to_owned()),
    }
}

fn invalid_report(bundle: PathBuf) -> ScanReport {
    ScanReport {
        bundle,
        architectures: Vst3Architecture::Unknown.as_str(),
        supported_on_apple_silicon: false,
        outcome: outcome_name(ScanOutcome::InvalidBundle),
        descriptors: Vec::new(),
        sdk_enumerate: false,
        detail: Some("path is not an inspectable VST3 bundle".to_owned()),
    }
}

const fn outcome_name(outcome: ScanOutcome) -> &'static str {
    match outcome {
        ScanOutcome::Supported => "supported",
        ScanOutcome::UnsupportedArchitecture => "unsupported_architecture",
        ScanOutcome::InvalidBundle => "invalid_bundle",
        ScanOutcome::TimedOut => "timed_out",
        ScanOutcome::Crashed => "crashed",
    }
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn parses_required_bundle_json_and_sdk_flag() {
        let args = parse_args(
            ["--bundle", "Example.vst3", "--json", "--sdk-enumerate"]
                .map(str::to_owned)
                .into_iter(),
        )
        .expect("arguments should parse");

        assert_eq!(args.bundle.to_string_lossy(), "Example.vst3");
        assert!(args.json);
        assert!(args.sdk_enumerate);
    }
}
