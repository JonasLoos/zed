> [!IMPORTANT]
> This is a fork of [zed](https://github.com/zed-industries/zed) with various adjustments, including an updated git diff viewer.

# Zed

Welcome to Zed, a high-performance, multiplayer code editor.

### Install

To build this fork, install the prerequisites for [macOS](docs/src/development/macos.md#dependencies), [Linux](docs/src/development/linux.md#dependencies), or [Windows](docs/src/development/windows.md#dependencies), then:

```sh
git clone https://github.com/JonasLoos/zed.git
cd zed
cargo run --release
```

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).
