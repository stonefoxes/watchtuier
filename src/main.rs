//! watchtuier — watch USB serial devices and network neighbours.
//!
//! Two front-ends over the same watchers:
//!   * tray (default): a system-tray icon with a live compact menu of every
//!     /dev/ttyUSB*, /dev/ttyACM* device and IP neighbours grouped by
//!     subnet, plus desktop notifications for freshly connected devices.
//!   * --tui: the legacy full-screen terminal UI.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::tty::IsTty;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use watchtuier::app::App;
use watchtuier::cli::{parse_args, Mode, USAGE};
use watchtuier::types::AppEvent;
use watchtuier::{network, ui, usb};

fn main() -> Result<()> {
    let mode = match parse_args(&std::env::args().skip(1).collect::<Vec<String>>()) {
        Ok(mode) => mode,
        Err(usage) => {
            eprint!("{usage}");
            std::process::exit(2);
        }
    };

    match mode {
        Mode::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Mode::Tui => run_tui(),
        #[cfg(target_os = "linux")]
        Mode::Tray { notify_net } => watchtuier::tray::run(notify_net),
        #[cfg(not(target_os = "linux"))]
        Mode::Tray { .. } => {
            anyhow::bail!("tray mode is Linux-only; run with --tui instead")
        }
    }
}

fn run_tui() -> Result<()> {
    if !io::stdout().is_tty() {
        anyhow::bail!("watchtuier must be run in an interactive terminal");
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal);

    // Restore terminal regardless of how we exited.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    if let Err(e) = result {
        eprintln!("watchtuier: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let (tx, rx) = mpsc::channel::<AppEvent>();

    let shutdown = Arc::new(AtomicBool::new(false));

    let usb_handle = usb::spawn_watcher(tx.clone(), shutdown.clone());
    let net_handle = network::spawn_watcher(tx, shutdown.clone());

    let mut app = App::new();

    // Initial render so the user sees the layout before any data arrives.
    terminal.draw(|f| ui::draw(f, &app))?;

    let result = event_loop(terminal, &mut app, &rx, &shutdown);

    // Signal background threads to exit.
    shutdown.store(true, Ordering::Release);
    // Don't block on join: udev monitor may be parked in recv() until its
    // SO_RCVTIMEO fires. Threads will terminate within ~1s.
    let _ = usb_handle.join();
    let _ = net_handle.join();

    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &mpsc::Receiver<AppEvent>,
    shutdown: &Arc<AtomicBool>,
) -> Result<()> {
    loop {
        // Drain all pending watcher events non-blockingly before each frame.
        let mut dirty = false;
        while let Ok(ev) = rx.try_recv() {
            app.apply(ev);
            dirty = true;
        }

        if dirty || app.quit {
            terminal.draw(|f| ui::draw(f, app))?;
        }

        if app.quit {
            return Ok(());
        }

        // Block up to 200ms for input; meanwhile the watchers keep producing
        // snapshots in the background. This keeps the UI responsive without
        // burning CPU on a tight spin.
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
                terminal.draw(|f| ui::draw(f, app))?;
                if app.quit {
                    // Ensure the shutdown flag flips so background threads see
                    // it promptly when we drop into `run()`'s join calls.
                    shutdown.store(true, Ordering::Release);
                    return Ok(());
                }
            }
        }
    }
}
