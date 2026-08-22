use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;

pub const USAGE: &str = "\
thylacine — snapshot-based backup

USAGE:
    thylacine init   --dest <DIR>          mark a directory as an archive
    thylacine index  --dest <DIR>          hash everything already in it
    thylacine backup --dest <DIR> <SRC>... copy sources into the archive
    thylacine help

Each source tree is replicated under its own name, so /home/tom/Pictures
lands in Pictures/. Two machines with a Pictures folder merge into one.

To restore, just open the archive and copy what you want — no tool needed.

Use -- to end flag parsing if a source path begins with a dash.
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Init {
        dest: PathBuf,
    },
    Backup {
        dest: PathBuf,
        sources: Vec<PathBuf>,
    },
    /// Walk the archive and record what is already there.
    Index {
        dest: PathBuf,
    },
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CliError {
    NoCommand,
    UnknownCommand(String),
    UnknownFlag(String),
    /// Flag was given as the last argument, or followed by another flag.
    MissingValue(&'static str),
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

/// Parse arguments *excluding* the program name.
///
/// Takes an iterator rather than reading `env::args_os()` directly so the
/// parser can be tested without a process.
pub fn parse<I>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();

    let command = match args.next() {
        Some(c) => c,
        None => return Err(CliError::NoCommand),
    };

    match command.to_str() {
        Some("init") => Ok(Command::Init {
            dest: dest_only(args)?,
        }),
        Some("backup") => parse_backup(args),
        Some("index") => Ok(Command::Index {
            dest: dest_only(args)?,
        }),
        Some("help") | Some("--help") | Some("-h") => Ok(Command::Help),
        _ => Err(CliError::UnknownCommand(
            command.to_string_lossy().into_owned(),
        )),
    }
}

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

/// Shared by `init` and `index`: exactly one `--dest`, no positionals.
fn dest_only<I>(mut args: I) -> Result<PathBuf, CliError>
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

    dest.ok_or(CliError::MissingFlag("--dest"))
}

/// Pull the value that follows a flag.
///
/// Refuses a value that itself looks like a flag, so `--dest --into /tmp`
/// fails on `--dest` rather than silently backing up to a directory named
/// `--into`.
fn value_for<I>(flag: &'static str, args: &mut I) -> Result<OsString, CliError>
where
    I: Iterator<Item = OsString>,
{
    match args.next() {
        Some(v) if !looks_like_flag(&v) => Ok(v),
        _ => Err(CliError::MissingValue(flag)),
    }
}

fn is_flag(s: &str) -> bool {
    s.starts_with('-') && s != "-"
}

fn looks_like_flag(s: &OsStr) -> bool {
    s.to_str().map(is_flag).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<Command, CliError> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn backup_collects_sources_after_dest() {
        let cmd = parse_str(&["backup", "--dest", "/mnt/backup", "/home/a", "/home/b"]).unwrap();
        assert_eq!(
            cmd,
            Command::Backup {
                dest: PathBuf::from("/mnt/backup"),
                sources: vec![PathBuf::from("/home/a"), PathBuf::from("/home/b")],
            }
        );
    }

    #[test]
    fn dest_may_appear_after_sources() {
        let cmd = parse_str(&["backup", "/home/a", "--dest", "/mnt/backup"]).unwrap();
        assert_eq!(
            cmd,
            Command::Backup {
                dest: PathBuf::from("/mnt/backup"),
                sources: vec![PathBuf::from("/home/a")],
            }
        );
    }

    #[test]
    fn last_dest_wins() {
        let cmd = parse_str(&["backup", "--dest", "/a", "--dest", "/b", "/src"]).unwrap();
        match cmd {
            Command::Backup { dest, .. } => assert_eq!(dest, PathBuf::from("/b")),
            other => panic!("expected backup, got {other:?}"),
        }
    }

    #[test]
    fn double_dash_ends_flag_parsing() {
        let cmd = parse_str(&["backup", "--dest", "/mnt/backup", "--", "--weird-dir"]).unwrap();
        match cmd {
            Command::Backup { sources, .. } => {
                assert_eq!(sources, vec![PathBuf::from("--weird-dir")]);
            }
            other => panic!("expected backup, got {other:?}"),
        }
    }

    #[test]
    fn flag_cannot_swallow_another_flag() {
        assert_eq!(
            parse_str(&["backup", "--dest", "--into", "/tmp", "/src"]),
            Err(CliError::MissingValue("--dest"))
        );
        assert_eq!(
            parse_str(&["backup", "--dest"]),
            Err(CliError::MissingValue("--dest"))
        );
    }

    #[test]
    fn backup_requires_dest_and_sources() {
        assert_eq!(
            parse_str(&["backup", "/home/a"]),
            Err(CliError::MissingFlag("--dest"))
        );
        assert_eq!(
            parse_str(&["backup", "--dest", "/mnt/backup"]),
            Err(CliError::NoSources)
        );
    }

    #[test]
    fn unknown_flags_and_commands_are_rejected() {
        assert_eq!(
            parse_str(&["backup", "--dset", "/mnt", "/src"]),
            Err(CliError::UnknownFlag("--dset".into()))
        );
        assert_eq!(
            parse_str(&["bakcup"]),
            Err(CliError::UnknownCommand("bakcup".into()))
        );
        assert_eq!(parse_str(&[]), Err(CliError::NoCommand));
    }

    #[test]
    fn index_takes_no_positionals() {
        assert_eq!(
            parse_str(&["index", "--dest", "/mnt/backup"]).unwrap(),
            Command::Index {
                dest: PathBuf::from("/mnt/backup")
            }
        );
        assert_eq!(
            parse_str(&["index", "--dest", "/mnt/backup", "extra"]),
            Err(CliError::UnexpectedArgument("extra".into()))
        );
    }

    #[test]
    fn init_takes_only_dest() {
        assert_eq!(
            parse_str(&["init", "--dest", "/mnt/backup"]).unwrap(),
            Command::Init {
                dest: PathBuf::from("/mnt/backup")
            }
        );
        assert_eq!(
            parse_str(&["init"]),
            Err(CliError::MissingFlag("--dest"))
        );
        assert_eq!(
            parse_str(&["init", "--dest", "/mnt/backup", "/src"]),
            Err(CliError::UnexpectedArgument("/src".into()))
        );
    }

    #[test]
    fn help_is_recognised_several_ways() {
        for form in ["help", "--help", "-h"] {
            assert_eq!(parse_str(&[form]).unwrap(), Command::Help);
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_source_paths_survive() {
        use std::os::unix::ffi::OsStringExt;

        // 0xFF is not valid UTF-8, but it is a perfectly legal Linux filename.
        let weird = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xFF]);
        let args = vec![
            OsString::from("backup"),
            OsString::from("--dest"),
            OsString::from("/mnt/backup"),
            weird.clone(),
        ];

        match parse(args).unwrap() {
            Command::Backup { sources, .. } => {
                assert_eq!(sources, vec![PathBuf::from(weird)]);
            }
            other => panic!("expected backup, got {other:?}"),
        }
    }
}