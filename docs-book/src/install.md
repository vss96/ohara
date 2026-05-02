# Install

ohara ships as two static binaries — `ohara` (the CLI) and `ohara-mcp`
(the MCP stdio server) — built per-platform by `cargo-dist` and
attached to every GitHub release.

## Supported platforms

| OS | Architectures |
|----|---------------|
| macOS | Apple silicon (`aarch64-apple-darwin`), Intel (`x86_64-apple-darwin`) |
| Linux | `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu` |
| Windows | not supported — use [WSL](https://learn.microsoft.com/en-us/windows/wsl/) |

## One-shot installer

The recommended path. Downloads the right binary for your platform,
drops it on `PATH`, and writes an install receipt that `ohara update`
later uses for self-update:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/vss96/ohara/releases/latest/download/ohara-cli-installer.sh | sh

curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/vss96/ohara/releases/latest/download/ohara-mcp-installer.sh | sh
```

Two installers because the CLI and the MCP server are independent
artifacts — most users want both, but you can install just one.

## Tarball download

If you'd rather not pipe a script:

1. Open the [releases page](https://github.com/vss96/ohara/releases).
2. Grab the `ohara-cli-*` and `ohara-mcp-*` tarball matching your
   platform.
3. Unpack and move the binaries somewhere on `PATH` (e.g.
   `/usr/local/bin` or `~/.local/bin`).

## Build from source

You need Rust 1.85 or newer (see `rust-toolchain.toml`). From a clone
of the repo:

```sh
cargo build --release --workspace
```

Both binaries land under `target/release/`.

### Build with hardware acceleration

The cargo-dist installer publishes a single CPU-only binary per
platform — same artifact for every host so the installer story stays
simple. To wire hardware ONNX execution providers into the embedder,
build from source with the matching cargo feature:

```sh
# Apple silicon — CoreML
cargo build --release --features coreml

# Linux x86_64 + NVIDIA — CUDA
cargo build --release --features cuda
```

The features flow through `ohara-embed` to both `ohara` and
`ohara-mcp`. Pair the resulting binary with
[`ohara index --embed-provider coreml`](./cli/index.md) (or `cuda`) —
or leave it on the default `auto`, which picks CoreML on Apple
silicon, CUDA when `CUDA_VISIBLE_DEVICES` is set, and CPU otherwise.
Default features stay CPU-only.

> **CoreML on long index runs (v0.6.1 workaround).** On Apple Silicon,
> the CoreML execution path leaks ~4 MB per `embed_batch` call
> (`MALLOC_LARGE` heap, see
> [`docs/perf/v0.6.1-leak-diagnosis.md`](https://github.com/vss96/ohara/blob/main/docs/perf/v0.6.1-leak-diagnosis.md)) —
> a 5,000+ commit first-time index would OOM the host before
> completing. v0.6.1's `--embed-provider auto` therefore resolves to
> CPU on Apple Silicon when the upcoming index pass would walk
> 1,000 commits or more; short passes (`query`, `index --incremental`,
> small repos) still pick CoreML. Pass `--embed-provider coreml`
> explicitly to bypass the downgrade and accept the OOM risk; CPU
> and CUDA paths are unaffected. Re-opened upstream investigation
> (fastembed / ort) is tracked for a future release.

## Updating

The CLI can self-update in place:

```sh
ohara update              # install the latest release
ohara update --check      # report whether a newer version exists
ohara update --prerelease # opt into pre-release tags
```

`ohara update` only works when the binary was installed via the
curl-pipe-sh installer above — it reads the install receipt that the
installer dropped beside the binary. If you built from source or
unpacked a tarball by hand, update by re-running the installer (or
re-building). The cargo-dist installer also drops a standalone
`ohara-cli-update` script alongside the binary; either entry point
works. See [`ohara update`](./cli/update.md) for the full flag set.

## Next

Now that the binaries are on `PATH`, head to the
[Quickstart](./quickstart.md) to index your first repo, or jump
straight to [Wiring into MCP clients](./mcp-clients.md) if you
already know the drill.
