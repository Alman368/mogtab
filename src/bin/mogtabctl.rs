use mogtab::ipc::{self, Direction, IpcCommand, OpenRequest, Profile};
use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedArgs {
    Command(IpcCommand),
    Help,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("mogtabctl: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let ParsedArgs::Command(command) = parse_args(env::args().skip(1))? else {
        println!("{}", help_text());
        return Ok(ExitCode::SUCCESS);
    };

    match command {
        IpcCommand::Open(_) => {
            ipc::send_or_start(command, &daemon_path()).map(|()| ExitCode::SUCCESS)
        }
        // Nothing to close when the daemon is not running.
        IpcCommand::Close | IpcCommand::Cancel => {
            let _ = ipc::send(command);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<ParsedArgs, String> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(ParsedArgs::Help);
    };

    match command.as_str() {
        "-h" | "--help" | "help" => Ok(ParsedArgs::Help),
        "close" => Ok(ParsedArgs::Command(IpcCommand::Close)),
        "cancel" => Ok(ParsedArgs::Command(IpcCommand::Cancel)),
        "open" => parse_open_args(args),
        _ => Err(format!("unknown command: {command}\n\n{}", help_text())),
    }
}

fn parse_open_args(args: impl IntoIterator<Item = String>) -> Result<ParsedArgs, String> {
    let mut profile = Profile::Alt;
    let mut direction = Direction::Next;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                let Some(value) = args.next() else {
                    return Err("--profile requires alt or super".to_string());
                };
                profile = parse_profile(&value)?;
            }
            "alt" | "super" => profile = parse_profile(&arg)?,
            "next" | "prev" => direction = parse_direction(&arg)?,
            "-h" | "--help" => return Ok(ParsedArgs::Help),
            _ => return Err(format!("unknown open argument: {arg}\n\n{}", help_text())),
        }
    }

    Ok(ParsedArgs::Command(IpcCommand::Open(OpenRequest {
        profile,
        direction,
    })))
}

fn parse_profile(value: &str) -> Result<Profile, String> {
    match value {
        "alt" => Ok(Profile::Alt),
        "super" => Ok(Profile::Super),
        _ => Err(format!("invalid profile: {value}")),
    }
}

fn parse_direction(value: &str) -> Result<Direction, String> {
    match value {
        "next" => Ok(Direction::Next),
        "prev" => Ok(Direction::Prev),
        _ => Err(format!("invalid direction: {value}")),
    }
}

/// The `mogtab` binary installed next to this one, else `mogtab` from `PATH`.
fn daemon_path() -> OsString {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("mogtab")))
        .filter(|sibling| sibling.is_file())
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("mogtab"))
}

fn help_text() -> String {
    "Usage:
  mogtabctl open [--profile alt|super] [next|prev]
  mogtabctl close
  mogtabctl cancel"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_alt_next() {
        assert_eq!(
            parse_args(["open"].into_iter().map(String::from)),
            Ok(ParsedArgs::Command(IpcCommand::Open(OpenRequest {
                profile: Profile::Alt,
                direction: Direction::Next,
            })))
        );
    }

    #[test]
    fn parses_existing_hotkey_shape() {
        assert_eq!(
            parse_args(
                ["open", "--profile", "super", "prev"]
                    .into_iter()
                    .map(String::from),
            ),
            Ok(ParsedArgs::Command(IpcCommand::Open(OpenRequest {
                profile: Profile::Super,
                direction: Direction::Prev,
            })))
        );
    }

    #[test]
    fn help_is_not_a_parse_error() {
        assert_eq!(
            parse_args(["--help"].into_iter().map(String::from)),
            Ok(ParsedArgs::Help)
        );
    }
}
