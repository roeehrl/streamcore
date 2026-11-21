//! Checks the adb provider against a real adb server: prints the server's
//! version and the ready cabled devices, and with a port opens one pipe to
//! that TCP port on the device, sends a line and prints what comes back.
//!
//!     adb_probe [--server 127.0.0.1:5037] [--port PORT [--serial SERIAL]]
//!
//! Device labels are printed here for the person at this machine only.

use std::time::Duration;
use streamcore_usb::adb::{AdbServer, AdbTarget, DEFAULT_SERVER};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (mut server, mut port, mut serial) = (DEFAULT_SERVER.to_owned(), None, None);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("an option is missing its value")?;
        match arg.as_str() {
            "--server" => server = value,
            "--port" => port = Some(value.parse::<u16>()?),
            "--serial" => serial = Some(value),
            other => return Err(format!("unknown option {other}").into()),
        }
    }
    let server = AdbServer::new(server.parse()?)?;
    println!("adb server version {:#x}", server.version().await?);
    let devices = server.devices().await?;
    println!("ready cabled devices: {}", devices.len());
    for device in &devices {
        println!("  {device}");
    }
    let Some(port) = port else { return Ok(()) };
    let serial = match serial {
        Some(serial) => serial,
        None if devices.len() == 1 => devices.into_iter().next().unwrap_or_default(),
        None => return Err("name one device with --serial".into()),
    };
    let started = std::time::Instant::now();
    let mut pipe = server.open(&serial, &AdbTarget::Tcp(port)).await?;
    println!(
        "pipe to tcp:{port} opened in {:.1} ms",
        started.elapsed().as_secs_f64() * 1e3
    );
    pipe.write_all(b"hello from the host\n").await?;
    let mut reply = vec![0; 256];
    let read = tokio::time::timeout(Duration::from_secs(3), pipe.read(&mut reply)).await??;
    println!(
        "device replied: {:?}",
        String::from_utf8_lossy(&reply[..read]).trim_end()
    );
    Ok(())
}
