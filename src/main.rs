use std::env;
use std::error::Error;
use std::process::ExitCode;

use thylacine::archive::Archive;
use thylacine::cache::Cache;
use thylacine::cli::{self, Command, USAGE};
use thylacine::engine::Engine;

fn main() -> ExitCode {
    let command = match cli::parse(env::args_os().skip(1)) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("thylacine: {err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("thylacine: {err}");
            let mut cause = err.source();
            while let Some(inner) = cause {
                eprintln!("  caused by: {inner}");
                cause = inner.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), Box<dyn Error>> {
    match command {
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }

        Command::Init { dest } => {
            let archive = Archive::init(&dest)?;
            println!("initialised archive at {}", dest.display());
            println!("  id: {}", archive.id());
            Ok(())
        }

        Command::Backup { dest, sources } => {
            // Archive first: it supplies the id the cache is keyed on.
            let archive = Archive::open(&dest)?;
            let cache = Cache::open(archive.id())?;

            let mut engine = Engine::new(archive, cache, sources);
            let stats = engine.run()?;
            println!("{stats}");
            Ok(())
        }

        // Adopt whatever is already in the archive: the first-run case for a
        // collection assembled by hand, and the recovery path after a lost
        // cache. `backup` does this automatically when the index is empty.
        Command::Index { dest } => {
            let archive = Archive::open(&dest)?;
            let cache = Cache::open(archive.id())?;
            let mut engine = Engine::new(archive, cache, Vec::new());
            let count = engine.reindex()?;
            println!("indexed {count} files in {}", dest.display());
            Ok(())
        }
    }
}