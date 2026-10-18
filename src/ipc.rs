//! Control protocol between the resident daemon and its clients.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

const SOCKET_NAME: &str = "mogtab.sock";
const AUTO_START_ATTEMPTS: u32 = 20;
const AUTO_START_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Profile {
    Alt,
    Super,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Direction {
    Next,
    Prev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenRequest {
    pub profile: Profile,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcCommand {
    Open(OpenRequest),
    Close,
    Cancel,
}

impl IpcCommand {
    pub fn payload(self) -> &'static str {
        match self {
            Self::Open(OpenRequest {
                profile: Profile::Alt,
                direction: Direction::Next,
            }) => "open alt next\n",
            Self::Open(OpenRequest {
                profile: Profile::Alt,
                direction: Direction::Prev,
            }) => "open alt prev\n",
            Self::Open(OpenRequest {
                profile: Profile::Super,
                direction: Direction::Next,
            }) => "open super next\n",
            Self::Open(OpenRequest {
                profile: Profile::Super,
                direction: Direction::Prev,
            }) => "open super prev\n",
            Self::Close => "close\n",
            Self::Cancel => "cancel\n",
        }
    }

    pub fn parse(payload: &str) -> Option<Self> {
        let open = |profile, direction| Some(Self::Open(OpenRequest { profile, direction }));
        match payload.trim() {
            "open next" | "open alt next" => open(Profile::Alt, Direction::Next),
            "open prev" | "open alt prev" => open(Profile::Alt, Direction::Prev),
            "open super next" => open(Profile::Super, Direction::Next),
            "open super prev" => open(Profile::Super, Direction::Prev),
            "close" => Some(Self::Close),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// Socket path in `$XDG_RUNTIME_DIR`, or in a private per-user directory under
/// the temp dir when there is no runtime dir, so other users cannot reach it.
pub fn socket_path() -> io::Result<PathBuf> {
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(runtime_dir).join(SOCKET_NAME));
    }

    let uid = fs::metadata("/proc/self")?.uid();
    let dir = env::temp_dir().join(format!("mogtab-{uid}"));
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
        _ => {}
    }

    let metadata = fs::symlink_metadata(&dir)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory of this user", dir.display()),
        ));
    }
    Ok(dir.join(SOCKET_NAME))
}

pub fn send(command: IpcCommand) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path()?)?;
    stream.write_all(command.payload().as_bytes())
}

/// Sends `command`, starting `<daemon> run` first when no daemon is listening.
pub fn send_or_start(command: IpcCommand, daemon: &OsStr) -> Result<(), String> {
    let Err(first_err) = send(command) else {
        return Ok(());
    };

    Command::new(daemon)
        .arg("run")
        .spawn()
        .map_err(|err| format!("failed to auto-start {}: {err}", daemon.to_string_lossy()))?;

    for _ in 0..AUTO_START_ATTEMPTS {
        thread::sleep(AUTO_START_RETRY_DELAY);
        if send(command).is_ok() {
            return Ok(());
        }
    }

    Err(format!(
        "daemon was not reachable after auto-start: {first_err}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_round_trip() {
        let commands = [
            IpcCommand::Open(OpenRequest {
                profile: Profile::Alt,
                direction: Direction::Next,
            }),
            IpcCommand::Open(OpenRequest {
                profile: Profile::Alt,
                direction: Direction::Prev,
            }),
            IpcCommand::Open(OpenRequest {
                profile: Profile::Super,
                direction: Direction::Next,
            }),
            IpcCommand::Open(OpenRequest {
                profile: Profile::Super,
                direction: Direction::Prev,
            }),
            IpcCommand::Close,
            IpcCommand::Cancel,
        ];

        for command in commands {
            assert_eq!(IpcCommand::parse(command.payload()), Some(command));
        }
    }

    #[test]
    fn legacy_open_payloads_use_alt_profile() {
        assert_eq!(
            IpcCommand::parse("open prev"),
            Some(IpcCommand::Open(OpenRequest {
                profile: Profile::Alt,
                direction: Direction::Prev,
            }))
        );
        assert_eq!(IpcCommand::parse("open sideways"), None);
    }
}
