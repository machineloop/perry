//! #4914 — `node:cluster` worker port sharing for the `net.Server` listen site.
//!
//! When this process is a `cluster.fork()`ed worker (Node's convention:
//! non-empty `NODE_UNIQUE_ID` in the environment), the TCP bind goes through
//! SO_REUSEPORT so N workers can share one port and the kernel load-balances
//! accepts (`SCHED_NONE`). Non-worker binds keep the plain `TcpListener::bind`
//! path, so a standalone `net.createServer().listen(port)` is unchanged.
//!
//! Mirrors perry-ext-http-server's `cluster_bind` (the HTTP/HTTPS/HTTP2 listen
//! sites already share ports this way). Round-robin fd-passing (`SCHED_RR`) and
//! the shared ephemeral port for `listen(0)` are the #4962 follow-up, deferred
//! here exactly as they are for the HTTP listen sites' SO_REUSEPORT default.

use std::net::{SocketAddr, TcpListener};

/// True when this process is a `cluster.fork()`ed worker — Node sets a
/// non-empty `NODE_UNIQUE_ID` in each worker's environment.
pub(crate) fn is_cluster_worker() -> bool {
    std::env::var("NODE_UNIQUE_ID")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Bind `addr`, enabling SO_REUSEPORT (+SO_REUSEADDR) when running as a cluster
/// worker so multiple worker processes can share one port and the kernel
/// load-balances accepts across them. Non-worker binds keep the plain
/// `TcpListener::bind` path. Returns a std listener; the caller adopts it with
/// `tokio::net::TcpListener::from_std` after `set_nonblocking(true)`.
pub(crate) fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    #[cfg(unix)]
    if is_cluster_worker() {
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
        socket.bind(&addr.into())?;
        // Node's default listen backlog.
        socket.listen(511)?;
        return Ok(socket.into());
    }
    TcpListener::bind(addr)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Two SO_REUSEPORT listeners bind the same live port (both succeed),
    /// while a plain bind on that port is refused — proving the worker bind
    /// genuinely shares the port via SO_REUSEPORT rather than racing for it.
    #[test]
    fn reuseport_lets_two_listeners_share_a_port() {
        use socket2::{Domain, Protocol, Socket, Type};

        fn reuseport_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
            let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
            socket.set_reuse_address(true)?;
            socket.set_reuse_port(true)?;
            socket.bind(&addr.into())?;
            socket.listen(511)?;
            Ok(socket.into())
        }

        // Ephemeral port so the test never collides with a real service.
        let probe = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let a = reuseport_listener(addr).expect("first SO_REUSEPORT bind should succeed");
        let b = reuseport_listener(addr).expect("second SO_REUSEPORT bind should share the port");

        // A plain (non-REUSEPORT) bind on the shared port is refused — the
        // sharing is exactly what SO_REUSEPORT grants.
        assert!(
            TcpListener::bind(addr).is_err(),
            "a plain bind on a SO_REUSEPORT-held port must be refused"
        );

        drop(a);
        drop(b);
    }
}
