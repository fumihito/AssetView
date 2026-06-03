# AssetView

AssetView is a quick desktop asset viewer for Windows.

This repository is intended to be built from a WSL2 Ubuntu environment.
The normal workflow uses the provided `./build` script, which cross-compiles
the app for Windows (`x86_64-pc-windows-gnu`).

## Build prerequisites

Before building, make sure the following packages are available on Ubuntu:

- `curl`
- `gcc-mingw-w64-x86-64`

The build script will install `rustup` automatically if it is not already
present in your environment.

If you want to prepare the machine manually, this is usually enough:

```bash
sudo apt update
sudo apt install -y curl gcc-mingw-w64-x86-64
```

## Build

Use the repository script for normal builds:

```bash
./build
```

Other useful variants:

```bash
./build debug
./build --rebuild
./build debug --rebuild
```

The release binary is written to:

`target/x86_64-pc-windows-gnu/release/AssetView.exe`

The debug binary is written to:

`target/x86_64-pc-windows-gnu/debug/AssetView.exe`

## Test

Run the Rust test suite with:

```bash
./tests/cargo-test.test
```

You can also run Cargo directly:

```bash
cargo test
```

## Checks

For the repository's broader validation pass, run:

```bash
./utils/check
```

This script is intended to bundle the project's local checks into one command.

## Notes

- The project uses Windows-specific functionality, so the primary build target
  is Windows even when the build is executed from WSL2.
- If you add new native dependencies, update the build instructions in this
  file and in `build` so they stay in sync.
