//! Checks the multiplexer provider against the real service: prints the
//! cabled devices, and with a port opens one pipe to that TCP port on a
//! device, sends a line and prints what comes back.
//!
//!     mux_probe [--tcp 127.0.0.1:27015] [--port PORT [--device LABEL]]
//!
//! Device labels are printed here for the person at this machine only.

use std::time::Duration;
use streamcore_usb::usbmux::{MuxAddress, MuxService};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (mut address, mut port, mut label) = (MuxAddress::platform_default(), None, None);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("an option is missing its value")?;
        match arg.as_str() {
            "--tcp" => address = MuxAddress::Tcp(value.parse()?),
            "--port" => port = Some(value.parse::<u16>()?),
            "--device" => label = Some(value),
            other => return Err(format!("unknown option {other}").into()),
        }
    }
    let service = MuxService::new(address, "mux_probe")?;
    let devices = service.devices().await?;
    println!("cabled devices: {}", devices.len());
    for (number, label) in &devices {
        println!("  {number}  {label}");
    }
    let Some(port) = port else { return Ok(()) };
    let number = match label {
        Some(label) => devices.iter().find(|(_, l)| *l == label).map(|(n, _)| *n),
        None if devices.len() == 1 => devices.first().map(|(n, _)| *n),
        None => return Err("name one device with --device".into()),
    }
    .ok_or("no such device")?;
    let started = std::time::Instant::now();
    let mut pipe = service.open(number, port).await?;
    println!(
        "pipe to port {port} opened in {:.1} ms",
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
