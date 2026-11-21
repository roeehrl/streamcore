//! The device side of a relayed pipe (STREAMCORE-VIDEO 8.4, device
//! listeners): the host's provider opens a pipe to a TCP port on the device's
//! loopback interface (through adb or the device multiplexer), so the device
//! listens there and dials the host over each pipe it accepts. The host is
//! always the TLS server.
//!
//! The port is open to every app on the device. That is safe only because the
//! pinned handshake runs on the pipe: whoever connects must hold the paired
//! host's key, or the dial fails and nothing is sent to it but a TLS hello.
//! Accepted pipes are bounded, so a local app cannot use them up.

use crate::UsbError;
use std::{
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use streamcore_transport::{channel::UsbPipe, transport::Medium};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};

/// Pipes a device holds at once: the live one, one being probed for a switch,
/// and room for a reconnect racing a close.
pub const MAX_DEVICE_PIPES: usize = 4;

/// A loopback listener for pipes the host opens to this device.
pub struct DeviceListener {
    listener: TcpListener,
    medium: Medium,
    pipes: Arc<Semaphore>,
}

impl DeviceListener {
    /// Listens on the first free port of `ports`, on the loopback interface
    /// only. The host's provider probes the same list in the same order, so a
    /// port another app holds moves both sides to the next one. `medium` is
    /// what carries the host's pipes to this device.
    pub async fn bind(ports: &[u16], medium: Medium) -> Result<Self, UsbError> {
        if ports.is_empty() || ports.contains(&0) || medium == Medium::UsbNet {
            return Err(UsbError::Protocol);
        }
        let mut last = None;
        for port in ports {
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, *port))).await {
                Ok(listener) => {
                    return Ok(Self {
                        listener,
                        medium,
                        pipes: Arc::new(Semaphore::new(MAX_DEVICE_PIPES)),
                    });
                }
                Err(error) => last = Some(error),
            }
        }
        Err(last.map_or(UsbError::Protocol, UsbError::Io))
    }

    /// The port this listener got.
    pub fn port(&self) -> Result<u16, UsbError> {
        Ok(self.listener.local_addr()?.port())
    }

    /// The next pipe: dial the host on it with
    /// `streamcore_transport::channel::connect_secure_stream_with`. While
    /// [`MAX_DEVICE_PIPES`] accepted pipes are still alive, further connections
    /// wait unaccepted. A pipe's place is freed when it is dropped.
    pub async fn accept(&self) -> Result<UsbPipe, UsbError> {
        let permit = self
            .pipes
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| UsbError::Protocol)?;
        let (stream, _) = self.listener.accept().await?;
        stream.set_nodelay(true)?;
        Ok(UsbPipe {
            io: Box::new(Counted {
                stream,
                _place: permit,
            }),
            medium: self.medium,
            // The peer of a device-side pipe is the host; there is no label.
            device: "host".into(),
        })
    }
}

/// An accepted pipe that holds one of the listener's places while it lives.
struct Counted {
    stream: TcpStream,
    _place: OwnedSemaphorePermit,
}

impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, data)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}
