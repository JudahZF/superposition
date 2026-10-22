//! `cargo xtask bundle-linux`: a relocatable Linux tarball and an AppImage of the same tree.
//!
//! The Linux binaries build in a Debian 12 container (glibc 2.36) against a source tree vendored
//! on the host, so the container needs no SSH access to the git dependencies. The Wine bridge's
//! unixlib builds there too; its PE half and the Windows helpers are portable and build on the
//! host. The pinned portable Wine build is downloaded and checked against its SHA-256.
//!
//! Layout: `bin/superposition` (launcher), `lib/superposition/{superposition, helpers/, wine/}`,
//! `share/doc/superposition/README-linux.md`.
//!
//! The AppImage is the pinned AppImage type-2 runtime followed by a zstd squashfs of that tree,
//! with `AppRun` linked to the launcher and the desktop entry and icon at its root. It needs
//! `mksquashfs` (the Nix dev shell provides it).

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

const IMAGE: &str = "superposition-linux-build:bookworm";
const WINE_URL: &str = "https://github.com/Kron4ek/Wine-Builds/releases/download/11.18/wine-11.18-staging-amd64-wow64.tar.xz";
const WINE_SHA256: &str = "15f312effe798838d4877a37126509618b3f98759744b37aeda0c6f55875d99a";
const APPIMAGE_RUNTIME_URL: &str =
    "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64";
const APPIMAGE_RUNTIME_SHA256: &str =
    "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d";
const LINUX_HELPERS: [&str; 3] = [
    "sp-plugin-worker",
    "sp-plugin-scanner",
    "sp-window-streamer",
];

fn run(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("could not run {command:?}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} failed with {status}"))
    }
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|error| format!("could not copy {}: {error}", from.display()))
}

fn sha256(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

/// Downloads a pinned file into `cache` once and verifies its SHA-256.
fn pinned_download(cache: &Path, name: &str, url: &str, expected: &str) -> Result<PathBuf, String> {
    let file = cache.join(name);
    if !file.is_file() || sha256(&file)? != expected {
        std::fs::create_dir_all(cache).map_err(|error| error.to_string())?;
        run(Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&file)
            .arg(url))?;
        let digest = sha256(&file)?;
        if digest != expected {
            return Err(format!("{name} has SHA-256 {digest}, expected {expected}"));
        }
    }
    Ok(file)
}

/// Packs `tree` (the tarball's top folder) into `<work>/<name>.AppImage`.
fn build_appimage(workspace_root: &Path, work: &Path, tree: &Path) -> Result<PathBuf, String> {
    let version = env!("CARGO_PKG_VERSION");
    let runtime = pinned_download(
        &work.join("cache"),
        "appimage-runtime-x86_64",
        APPIMAGE_RUNTIME_URL,
        APPIMAGE_RUNTIME_SHA256,
    )?;
    let packaging = workspace_root.join("packaging/linux");
    let image = work.join("appimage.squashfs");
    let _ = std::fs::remove_file(&image);
    // Pseudo files add the AppImage's root entries without copying the tree.
    let pseudo = [
        "AppRun s 755 0 0 bin/superposition".to_owned(),
        format!(
            "superposition.desktop f 644 0 0 cat {}",
            packaging.join("superposition.desktop").display()
        ),
        format!(
            "superposition.png f 644 0 0 cat {}",
            packaging.join("superposition.png").display()
        ),
        ".DirIcon s 644 0 0 superposition.png".to_owned(),
    ];
    let mut mksquashfs = Command::new("mksquashfs");
    mksquashfs
        .arg(tree)
        .arg(&image)
        .args(["-comp", "zstd", "-root-owned", "-noappend", "-quiet"]);
    for definition in &pseudo {
        mksquashfs.arg("-p").arg(definition);
    }
    run(&mut mksquashfs)?;
    let appimage = work.join(format!("Superposition-{version}-x86_64.AppImage"));
    let mut bytes = std::fs::read(&runtime).map_err(|error| error.to_string())?;
    bytes.extend(std::fs::read(&image).map_err(|error| error.to_string())?);
    std::fs::write(&appimage, bytes).map_err(|error| error.to_string())?;
    std::fs::remove_file(&image).map_err(|error| error.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&appimage, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    Ok(appimage)
}

fn id(flag: &str) -> Result<String, String> {
    let output = Command::new("id")
        .arg(flag)
        .output()
        .map_err(|error| format!("could not run id: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(crate) fn run_bundle(workspace_root: &Path, arguments: &[String]) -> Result<(), String> {
    if !arguments.is_empty() {
        return Err("bundle-linux does not accept arguments".to_owned());
    }
    let work = workspace_root.join("target/bundle-linux");
    build_in_container(workspace_root, &work)?;

    let version = env!("CARGO_PKG_VERSION");
    let name = format!("superposition-{version}-linux-x86_64");
    let staging = work.join(&name);
    let _ = std::fs::remove_dir_all(&staging);
    let lib = staging.join("lib/superposition");
    let helpers = lib.join("helpers");
    let built = work.join("target/release");
    copy(&built.join("superposition"), &lib.join("superposition"))?;
    for helper in LINUX_HELPERS {
        copy(&built.join(helper), &helpers.join(helper))?;
    }
    // The Windows helpers and the bridge's PE half are portable and build on the host; the
    // unixlib is replaced by the container build so it needs only the older glibc.
    crate::wine::run_windows_helpers(
        workspace_root,
        &[
            "--release".to_owned(),
            "--out".to_owned(),
            helpers.display().to_string(),
        ],
    )?;
    copy(
        &work.join("target/sp_wine_bridge.so"),
        &helpers.join("wine-bridge/x86_64-unix/sp_wine_bridge.so"),
    )?;
    copy(
        &workspace_root.join("packaging/linux/superposition.sh"),
        &staging.join("bin/superposition"),
    )?;
    copy(
        &workspace_root.join("packaging/linux/README-linux.md"),
        &staging.join("share/doc/superposition/README-linux.md"),
    )?;
    let wine = pinned_download(
        &work.join("cache"),
        "wine-11.18-staging-amd64-wow64.tar.xz",
        WINE_URL,
        WINE_SHA256,
    )?;
    std::fs::create_dir_all(lib.join("wine")).map_err(|error| error.to_string())?;
    run(Command::new("tar")
        .arg("-xJf")
        .arg(&wine)
        .args(["--strip-components=1", "-C"])
        .arg(lib.join("wine")))?;
    let tarball = work.join(format!("{name}.tar.xz"));
    run(Command::new("tar")
        .args(["-cJf"])
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .arg(&name))?;
    println!("built {}", tarball.display());
    let appimage = build_appimage(workspace_root, &work, &staging)?;
    println!("built {}", appimage.display());
    Ok(())
}

/// Vendors the sources on the host, then builds the Linux binaries and the bridge unixlib in
/// the Debian 12 container into `<work>/target`.
fn build_in_container(workspace_root: &Path, work: &Path) -> Result<(), String> {
    let vendor = work.join("vendor");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    // Vendor on the host, where git can reach the SSH dependencies.
    let vendored = Command::new(&cargo)
        .current_dir(workspace_root)
        .args(["vendor", "--locked", "--versioned-dirs"])
        .arg(&vendor)
        .output()
        .map_err(|error| format!("could not run cargo vendor: {error}"))?;
    if !vendored.status.success() {
        return Err(format!(
            "cargo vendor failed: {}",
            String::from_utf8_lossy(&vendored.stderr)
        ));
    }
    // The container mounts the workspace at /src.
    let config = String::from_utf8_lossy(&vendored.stdout)
        .replace(&workspace_root.display().to_string(), "/src");
    std::fs::write(work.join("vendor-config.toml"), config).map_err(|error| error.to_string())?;

    run(Command::new("docker")
        .args(["build", "--tag", IMAGE, "--file"])
        .arg(workspace_root.join("packaging/linux/Dockerfile"))
        .arg(workspace_root.join("packaging/linux")))?;
    let packages: Vec<String> = ["superposition"]
        .iter()
        .chain(LINUX_HELPERS.iter())
        .flat_map(|package| ["-p".to_owned(), (*package).to_owned()])
        .collect();
    let script = format!(
        "set -e; mkdir -p .cargo-bundle; cp target/bundle-linux/vendor-config.toml \
         .cargo-bundle/config.toml; \
         CARGO_HOME=/src/target/bundle-linux/cargo-home cargo build --release --locked --offline \
         --config .cargo-bundle/config.toml {packages}; \
         cc -O2 -shared -fPIC -Wall -Werror -o target/bundle-linux/target/sp_wine_bridge.so \
         crates/sp-shared-memory-os/wine/sp_wine_bridge_unix.c; rm -rf .cargo-bundle",
        packages = packages.join(" "),
    );
    // Build as the invoking user so the outputs are not owned by root.
    let user = format!("{}:{}", id("-u")?, id("-g")?);
    run(Command::new("docker")
        .args(["run", "--rm", "--user", &user])
        .arg("--volume")
        .arg(format!("{}:/src", workspace_root.display()))
        .args(["--workdir", "/src"])
        .args(["--env", "CARGO_TARGET_DIR=/src/target/bundle-linux/target"])
        .args([IMAGE, "sh", "-c", &script]))
}
