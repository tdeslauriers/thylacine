# thylacine

A snapshot-based backup tool written in Rust.

- **Status: early development. Do not use this for data you care about.**

## Name

The thylacine was declared extinct in 1936 because nobody kept a copy.

## Why

Automating a chore and a backing up data has some interesting learning challenges to work thru like atomicity,
interrupted runs, renames, symlinks, permissions, and knowing what changed
without reading everything.

## Usage

```
thylacine init   --dest <DIR>          mark a directory as an archive
thylacine index  --dest <DIR>          hash everything already in it
thylacine backup --dest <DIR> <SRC>... copy sources into the archive
thylacine help
```

## Design

**The archive must be readable without this tool.** This is the constraint everything else bends around. Files land in 
ordinary directories under their real names, so anyone in the house can plug in the drive, open a folder, and drag out 
what they need. No software, no database, no explanation.

**Identity is content, not path.** Every file is identified by the SHA-256 of its bytes. Before copying anything, thylacine asks whether 
those bytes already exist anywhere in the archive — any folder, any filename. This is what makes reorganising 
the archive by hand safe, and what catches the same photo arriving from two machines under two names.

**The database is a cache, not the source of truth.** It holds two tables, both rebuildable by walking and re-hashing:

* sources — what each source file looked like last run, so unchanged files are skipped without being opened
* archived — what is currently in the archive, so duplicate content is recognised

If the database is deleted, the next run will be slow slow, because everything will be re-hashed, but nothing will be lost.

**Nothing is ever overwritten.** An edited document produces a new hash, so it is archived alongside the old version rather than replacing it. 
Two different files with the same name are disambiguated with a prefix of the content hash — IMG_1234-a19d4c7e.jpg

## Building

```sh
cargo build --release
```

`rusqlite` is used with the `bundled` feature, so SQLite is compiled from source and
there is no system library to install. A C compiler is required.
