//! `TCP_NOTSENT_LOWAT` on TLS/TCP sockets (docs/STREAMCORE-VIDEO.md 6.2). This is
//! the one place in the crate that may use `unsafe`: Rust's standard library and
//! socket2 do not set this option on Apple platforms, so it calls `setsockopt`.
//!
//! Windows has no such option. Its equivalent (6.2) is a Rust-tracked unsent queue
//! with `SIO_TCP_INFO` and a small `SO_SNDBUF`; until that lands (P5) the option
//! is ignored there. TODO(P5): Windows unsent-byte bound.
use tokio::net::TcpStream;

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
pub(crate) fn set_notsent_lowat(stream: &TcpStream, bytes: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // <netinet/tcp.h>: 0x201 on Apple platforms; libc exports the Linux value.
    #[cfg(target_vendor = "apple")]
    const TCP_NOTSENT_LOWAT: libc::c_int = 0x201;
    #[cfg(not(target_vendor = "apple"))]
    const TCP_NOTSENT_LOWAT: libc::c_int = libc::TCP_NOTSENT_LOWAT;
    let value = libc::c_int::try_from(bytes)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    #[allow(unsafe_code)]
    // SAFETY: the descriptor belongs to `stream`, which is borrowed for the call
    // and so stays open; the option value is a live `c_int` whose exact size is
    // passed as its length, and the kernel only reads it.
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            TCP_NOTSENT_LOWAT,
            (&raw const value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
pub(crate) fn set_notsent_lowat(_stream: &TcpStream, _bytes: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(
    test,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_low_water_mark_is_accepted_by_the_kernel() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        set_notsent_lowat(&stream, 64 * 1024).unwrap();
        assert!(set_notsent_lowat(&stream, u32::MAX).is_err());
    }
}
