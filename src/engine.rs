use crate::catalog::Catalog;
use std::{path::PathBuf, println};

pub struct Engine {
    catalog: Catalog,
    sources: Vec<PathBuf>,
    dest: PathBuf,
}

impl Engine {
    pub fn new(catalog: Catalog, sources: Vec<PathBuf>, dest: PathBuf) -> Self {
        Engine {
            catalog,
            sources,
            dest,
        }
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // stub
        println!("backing up {:?} -> {:?}", self.sources, self.dest);
        Ok(())
    }
}
