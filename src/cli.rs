//! Command-line parsing shared by the tray and TUI front-ends.

/// How to run the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// System-tray app: live menu + notifications (the default).
    Tray {
        /// Also notify when a new network neighbour appears.
        notify_net: bool,
    },
    /// Legacy terminal UI.
    Tui,
    /// Print usage and exit successfully.
    Help,
}

pub const USAGE: &str = "\
watchtuier — watch USB serial devices and network neighbours

USAGE:
    watchtuier [OPTIONS]

OPTIONS:
    --notify-net   Also notify when a new network neighbour appears
                   (USB serial connect/disconnect always notifies)
    --tui          Run the legacy terminal UI instead of the tray app
    -h, --help     Print this help

By default watchtuier lives in the system tray: click the icon to see all
/dev/ttyUSB*, /dev/ttyACM* devices and IP neighbours grouped by subnet.
";

/// Parse arguments (without the program name). Unknown flags produce an
/// error carrying [`USAGE`].
pub fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut tui = false;
    let mut notify_net = false;

    for arg in args {
        match arg.as_str() {
            "--tui" => tui = true,
            "--notify-net" => notify_net = true,
            "-h" | "--help" => return Ok(Mode::Help),
            other => return Err(format!("unknown option: {other}\n\n{USAGE}")),
        }
    }

    if tui && notify_net {
        return Err(format!(
            "--notify-net only applies to tray mode, not --tui\n\n{USAGE}"
        ));
    }

    Ok(if tui {
        Mode::Tui
    } else {
        Mode::Tray { notify_net }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_means_tray_without_net_notifications() {
        assert_eq!(parse_args(&args(&[])), Ok(Mode::Tray { notify_net: false }));
    }

    #[test]
    fn notify_net_opts_in_to_neighbour_notifications() {
        assert_eq!(
            parse_args(&args(&["--notify-net"])),
            Ok(Mode::Tray { notify_net: true })
        );
    }

    #[test]
    fn tui_flag_selects_legacy_ui() {
        assert_eq!(parse_args(&args(&["--tui"])), Ok(Mode::Tui));
    }

    #[test]
    fn help_flags_short_circuit() {
        assert_eq!(parse_args(&args(&["--help"])), Ok(Mode::Help));
        assert_eq!(parse_args(&args(&["-h"])), Ok(Mode::Help));
    }

    #[test]
    fn unknown_flag_is_an_error_with_usage() {
        let err = parse_args(&args(&["--frobnicate"])).unwrap_err();
        assert!(err.contains("--frobnicate"));
        assert!(err.contains("USAGE"));
    }

    #[test]
    fn tui_and_notify_net_conflict() {
        let err = parse_args(&args(&["--tui", "--notify-net"])).unwrap_err();
        assert!(err.contains("--notify-net only applies to tray mode"));
    }
}
