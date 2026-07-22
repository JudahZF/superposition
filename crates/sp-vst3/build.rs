#![allow(missing_docs)]

fn main() {
    println!("cargo:rerun-if-env-changed=VST3_SDK_DIR");
    println!("cargo:rerun-if-changed=src/native/vst3_shim.cpp");
    println!("cargo:rerun-if-changed=src/native/vst3_shim.h");

    if std::env::var_os("CARGO_FEATURE_NATIVE_SDK").is_none() {
        return;
    }

    assert!(
        std::env::consts::OS == "macos",
        "sp-vst3 native-sdk is currently supported only on macOS"
    );

    let sdk = std::env::var_os("VST3_SDK_DIR").map_or_else(|| {
        panic!("sp-vst3 native-sdk requires VST3_SDK_DIR to name the VST3 SDK root (for example /Users/judahfuller/SDKs/vst3sdk)")
    }, std::path::PathBuf::from);
    let required = [
        "pluginterfaces/base/funknown.h",
        "pluginterfaces/vst/ivstcomponent.h",
        "pluginterfaces/vst/ivstaudioprocessor.h",
        "pluginterfaces/vst/ivsteditcontroller.h",
        "pluginterfaces/gui/iplugview.h",
        "public.sdk/source/common/memorystream.h",
    ];
    for relative in required {
        assert!(
            sdk.join(relative).is_file(),
            "sp-vst3 native-sdk could not find {relative} beneath VST3_SDK_DIR={}; install the official VST3 SDK or disable native-sdk",
            sdk.display()
        );
    }

    cc::Build::new()
        .cpp(true)
        .file("src/native/vst3_shim.cpp")
        .file(sdk.join("pluginterfaces/base/funknown.cpp"))
        .file(sdk.join("pluginterfaces/base/ustring.cpp"))
        .file(sdk.join("public.sdk/source/common/memorystream.cpp"))
        .file(sdk.join("public.sdk/source/vst/hosting/hostclasses.cpp"))
        .file(sdk.join("public.sdk/source/vst/hosting/pluginterfacesupport.cpp"))
        .include(&sdk)
        .flag_if_supported("-std=c++17")
        .warnings(true)
        .compile("sp_vst3_native_sdk");
}
