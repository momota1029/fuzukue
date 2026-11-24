use std::io;
use std::net::{Ipv4Addr, TcpListener};

pub(crate) fn listen() -> io::Result<Option<TcpListener>> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => Ok(Some(listener)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::AddrNotAvailable
                    | io::ErrorKind::Unsupported
            ) =>
        {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("TCP test");
            eprintln!("SKIPPED: {name}: loopback TCP unavailable ({error})");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
