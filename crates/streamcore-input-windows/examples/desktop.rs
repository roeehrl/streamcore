//! Prints what every function of `streamcore_input_windows::desktop` reads, for a look at a real
//! Windows desktop:
//!
//! ```text
//! cargo run -p streamcore-input-windows --example desktop
//! cargo run -p streamcore-input-windows --example desktop -- --watch 30
//! cargo run -p streamcore-input-windows --example desktop -- --activate 2
//! ```
//!
//! With no arguments it prints once and changes nothing. `--watch <seconds>` then polls for that
//! long and prints a line whenever the foreground app, the focused layout or the session's state
//! changes (switch windows, switch layouts, lock the workstation). `--activate <n>` asks Windows
//! to bring the n-th listed window to the foreground and prints whether it agreed; it is the one
//! thing here that changes anything. `--out <path>` writes the same lines to a file, for a
//! launch with no console.
//!
//! Run from a service or over SSH there is no interactive desktop, and it says so: no foreground
//! app, no windows, a `Detached` session.

#[cfg(not(windows))]
fn main() {
    println!("desktop: this example reads the Windows desktop; nothing was run");
}

#[cfg(windows)]
fn main() {
    show::run();
}

#[cfg(windows)]
mod show {
    use std::fs::File;
    use std::io::Write;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use streamcore_input_windows::desktop::{
        self, ForegroundApp, LayoutId, SessionState, labelled_keys,
    };

    struct Out(Option<File>);

    impl Out {
        fn line(&mut self, text: &str) {
            println!("{text}");
            if let Some(file) = &mut self.0 {
                let _ = writeln!(file, "{text}");
            }
        }
    }

    fn path(path: Option<&std::path::Path>) -> String {
        path.map_or_else(|| "(unknown)".to_owned(), |p| p.display().to_string())
    }

    fn foreground(app: Option<&ForegroundApp>) -> String {
        match app {
            Some(app) => format!(
                "pid {} {} {:?} {:?}",
                app.pid,
                path(app.exe_path.as_deref()),
                app.window_title,
                app.window
            ),
            None => "none".to_owned(),
        }
    }

    fn print_layout(out: &mut Out) {
        let Some(layout) = desktop::focused_layout() else {
            out.line("focused_layout: none");
            return;
        };
        out.line(&format!(
            "focused_layout: id {} name {:?} locale {} family {} ({} keys labelled)",
            layout.id,
            layout.name,
            layout.locale.as_deref().unwrap_or("(none)"),
            layout.short.as_deref().unwrap_or("(other)"),
            layout.labels.len()
        ));
        // The labels in HID order, eight to a line: usage=base/shifted.
        let labels: Vec<String> = labelled_keys()
            .filter_map(|hid| {
                let label = layout.labels.get(&hid.to_string())?;
                Some(format!("{hid}={}", label.join("/")))
            })
            .collect();
        for row in labels.chunks(8) {
            out.line(&format!("  {}", row.join("  ")));
        }
    }

    fn print_all(out: &mut Out) {
        out.line(&format!("os_version: {}", desktop::os_version()));
        out.line(&format!("session_state: {:?}", desktop::session_state()));
        out.line(&format!(
            "focused_layout_id: {:?}",
            desktop::focused_layout_id()
        ));
        print_layout(out);
        out.line(&format!(
            "foreground_app: {}",
            foreground(desktop::foreground_app().as_ref())
        ));
        let apps = desktop::running_apps();
        out.line(&format!("running_apps: {}", apps.len()));
        for (index, app) in apps.iter().enumerate() {
            out.line(&format!(
                "  {index}: pid {} {}{} {:?} {:?}",
                app.pid,
                path(app.exe_path.as_deref()),
                if app.minimized { " (minimized)" } else { "" },
                app.title,
                app.window
            ));
        }
    }

    fn activate(out: &mut Out, index: usize) {
        let apps = desktop::running_apps();
        let Some(app) = apps.get(index) else {
            out.line(&format!("activate: no window {index} in the list"));
            return;
        };
        let agreed = desktop::activate(app.window);
        sleep(Duration::from_millis(300));
        let now = desktop::foreground_app();
        out.line(&format!(
            "activate {index} ({:?}): {agreed}; in the foreground afterwards: {}",
            app.title,
            now.is_some_and(|front| front.window == app.window)
        ));
    }

    fn watch(out: &mut Out, seconds: u64) {
        out.line(&format!("watching for {seconds} s"));
        let started = Instant::now();
        let mut last: Option<(Option<ForegroundApp>, Option<LayoutId>, SessionState)> = None;
        while started.elapsed() < Duration::from_secs(seconds) {
            let now = (
                desktop::foreground_app(),
                desktop::focused_layout_id(),
                desktop::session_state(),
            );
            if last.as_ref() != Some(&now) {
                let layout = desktop::focused_layout();
                out.line(&format!(
                    "{:6.1} s  {:?}  layout {} ({:?})  foreground {}",
                    started.elapsed().as_secs_f64(),
                    now.2,
                    layout.as_ref().map_or("none", |l| l.id.as_str()),
                    now.1,
                    foreground(now.0.as_ref())
                ));
                last = Some(now);
            }
            sleep(Duration::from_millis(200));
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let value = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
        };
        let mut out = Out(value("--out").and_then(|path| File::create(path).ok()));
        print_all(&mut out);
        if let Some(index) = value("--activate").and_then(|n| n.parse().ok()) {
            activate(&mut out, index);
        }
        if let Some(seconds) = value("--watch").and_then(|n| n.parse().ok()) {
            watch(&mut out, seconds);
        }
    }
}
