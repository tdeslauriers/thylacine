use std::error::Error;
use std::path::PathBuf;
use thylacine::catalog::Catalog;
use thylacine::engine::Engine;

fn main() -> Result<(), Box<dyn Error>> {
    let catalog = Catalog::open(&PathBuf::from("index.db"))?;

    let mut engine = Engine::new(
        catalog,
        vec![PathBuf::from("/home/atomic/Documents")],
        PathBuf::from("/mnt/backup"),
    );

    engine.run()?;
    Ok(())
}
