use core::fmt;
use std::{
    ffi::{OsStr, OsString},
    fmt::DebugStruct,
    path::{Path, PathBuf},
};

use rusqlite::types::FromSqlError::Other;

pub const USAGE: &str = "\
thylacine — snapshot-based backup
 
USAGE:
    thylacine backup  --dest <DIR> <SOURCE>...
    thylacine verify  --dest <DIR>
    thylacine restore --dest <DIR> [--snapshot <ID>] --into <DIR>
    thylacine help
 
Use -- to end flag parsing if a source path begins with a dash.
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Backup {
        dest: PathBuf,
        sources: Vec<PathBuf>,
    },
    Verify {
        dest: PathBuf,
    },
    Restore {
        dest: PathBuf,
        snapshot: String,
        into: PathBuf,
    },
    Help,
}

// Backup
// parses the backup arguments
fn parse_backup<I>(mut args: I) -> Result<Command, CliError>
where
    I: Iterator<Item = OsString>,
{
    let mut dest: Option<PathBuf> = None;
    let mut sources: Vec<PathBuf> = Vec::new();
    let mut flags_done = false;

    while let Some(arg) = args.next() {
        if !flags_done {
            match arg.to_str() {
                Some("--") => {
                    flags_done = true;
                    continue;
                }
                Some("--dest") => {
                    dest = Some(value_for("--dest", &mut args)?.into());
                    continue;
                }
                Some(other) if is_flag(other) => {
                    return Err(CliError::UnknownFlag(other.to_string()));
                }
                _ => {}
            }
        }
        sources.push(PathBuf::from(arg));
    }

    let dest = dest.ok_or(CliError::MissingFlag("--dest"))?;
    if sources.is_empty() {
        return Err(CliError::NoSources);
    }

    Ok(Command::Backup { dest, sources })
}

fn parse_verify<I>(mut args: I) -> Result<Command, CliError>
where
    I: Iterator<Item = OsString>,
{
    let mut dest: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--dest") => dest = Some(value_for("--dest", &mut args)?.into()),
            Some(other) if is_flag(other) => {
                return Err(CliError::UnknownFlag(other.to_string()));
            }
            _ => {
                return Err(CliError::UnexpectedArgument(
                    arg.to_string_lossy().into_owned(),
                ));
            }
        }
    }

    Ok(Command::Verify {
        dest: dest.ok_or(CliError::MissingFlag("--dest"))?,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum CliError {
    NoCommand,
    UnknownCommand(String),
    UnknownFlag(String),
    MissingValue(&'static str), // for example a --flag followed by nothing or by another --flag
    MissingFlag(&'static str),
    NoSources,
    UnexpectedArgument(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::NoCommand => write!(f, "no command given"),
            CliError::UnknownCommand(c) => write!(f, "unknown command: {c}"),
            CliError::UnknownFlag(flag) => write!(f, "unknown flag: {flag}"),
            CliError::MissingValue(flag) => write!(f, "{flag} requires a value"),
            CliError::MissingFlag(flag) => write!(f, "{flag} is required"),
            CliError::NoSources => write!(f, "at least one source path is required"),
            CliError::UnexpectedArgument(a) => write!(f, "unexpected argument: {a}"),
        }
    }
}

impl std::error::Error for CliError {}

// gets the value following a flag.
// errors if the value looks like a flag
fn value_for<I>(flag: &'static str, args: &mut I) -> Result<OsString, CliError>
where
    I: Iterator<Item = OsString>,
{
    match args.next() {
        Some(v) if !looks_like_flag(&v) => Ok(v),
        _ => Err(CliError::MissingValue(flag)),
    }
}

// helper function to check if a argument is a flag or not.
fn is_flag(s: &str) -> bool {
    s.starts_with('-') && s != "-"
}

// helper function to check if an arugment looks like a flag vs a file or directory
fn looks_like_flag(s: &OsStr) -> bool {
    s.to_str().map(is_flag).unwrap_or(false)
}
