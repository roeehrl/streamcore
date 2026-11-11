//! `streamcore-video-windows-bench`: adapter report, encoder probe and the per-stage bench. See
//! the crate documentation and `cli::USAGE`.

#[cfg(all(windows, feature = "bench"))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match streamcore_video_windows::cli::parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n{}", streamcore_video_windows::cli::USAGE);
            std::process::exit(2);
        }
    };
    if let Err(e) = streamcore_video_windows::win::run(&options) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(all(windows, feature = "bench")))]
fn main() {
    eprintln!("streamcore-video-windows-bench runs only on Windows, built with --features bench.");
    std::process::exit(2);
}
