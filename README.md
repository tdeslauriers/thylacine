# thylacine

A snapshot-based backup tool written in Rust.

- **Status: early development. Do not use this for data you care about.**

## Name

The thylacine was declared extinct in 1936 because nobody kept a copy.

## Why

Automating a chore. The challenge of backing up data goes well beyond "copy the bytes."
There is atomicity, interrupted runs, renames, symlinks, permissions, and knowing what changed
without reading everything. `thylacine` is a learning project to work through those problems directly.

## Design

**Snapshots vs a mirror.** A mirror faithfully propagates corruption, accidental
deletion, and ransomware-encrypted files to the only copy you have. Each `thylacine` run produces a
point-in-time snapshot, and old snapshots are kept under a retention policy rather than
overwritten.

## Building

```sh
cargo build --release
```

`rusqlite` is used with the `bundled` feature, so SQLite is compiled from source and
there is no system library to install. A C compiler is required.

## Usage

Nothing is usable yet.
