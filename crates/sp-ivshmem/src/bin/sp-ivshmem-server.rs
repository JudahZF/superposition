//! Serves one VM port: `sp-ivshmem-server --socket PATH --size BYTES [--vectors N]`.
//!
//! QEMU connects with `-chardev socket,path=PATH,id=…` and
//! `-device ivshmem-doorbell,chardev=…,vectors=N`. Linux only.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::{path::PathBuf, process::ExitCode};

    const USAGE: &str = "usage: sp-ivshmem-server --socket PATH --size BYTES [--vectors N]";

    let mut socket = None;
    let mut size = None;
    let mut vectors = 1;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next();
        let parsed = match (flag.as_str(), value) {
            ("--socket", Some(value)) => {
                socket = Some(PathBuf::from(value));
                true
            }
            ("--size", Some(value)) => value.parse().map(|value| size = Some(value)).is_ok(),
            ("--vectors", Some(value)) => value.parse().map(|value| vectors = value).is_ok(),
            _ => false,
        };
        if !parsed {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    }
    let (Some(socket), Some(size)) = (socket, size) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let mut server = match sp_ivshmem::PortServer::bind(&socket, size, vectors) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("sp-ivshmem-server: {}: {error}", socket.display());
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "sp-ivshmem-server: {} serves {} bytes, {vectors} vectors",
        socket.display(),
        server.size()
    );
    eprintln!("sp-ivshmem-server: {}", server.run());
    ExitCode::FAILURE
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("sp-ivshmem-server runs only on Linux");
    std::process::ExitCode::FAILURE
}
