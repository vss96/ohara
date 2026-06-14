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

The cargo-dist installer for `aarch64-apple-darwin` (Apple Silicon)
bundles the CoreML execution provider from v0.6.2 onwards — you no
longer need to rebuild from source to get hardware acceleration on
Apple Silicon. CUDA on Linux still requires a source rebuild:

```sh
# Linux x86_64 + NVIDIA — CUDA
cargo build --release --features cuda

# Apple silicon — CoreML (only needed if you want CoreML on a
# from-source build; the released binary already has it)
cargo build --release --features coreml
```

The features flow through `ohara-embed` to both `ohara` and
`ohara-mcp`. The default `auto` picks CUDA when `CUDA_VISIBLE_DEVICES`
is set, then CoreML on a CoreML-capable macOS build (the released macOS
binary, or a source build with `--features coreml`), then CPU — so on
Apple silicon a plain `ohara index` indexes on CoreML automatically.
Pin a provider with [`ohara index --embed-provider {cpu,coreml,cuda}`](./cli/index.md)
to override. Default source-build features stay CPU-only, so a build
without `--features coreml` keeps `auto` on CPU.

> **CoreML rework (v0.11).** Earlier releases auto-downgraded CoreML
> to CPU on long index passes because the dynamic-shape CoreML path
> leaked ~4 MB per batch and could OOM the host. v0.11 replaced that
> path with a fixed-shape fp32 model that runs on the GPU+Neural
> Engine at ~3× CPU throughput with a flat memory footprint (the
> "leak" was CoreML re-specializing per tensor shape — see
> [`docs/perf/v0.11-coreml-fixed-shape.md`](https://github.com/vss96/ohara/blob/main/docs/perf/v0.11-coreml-fixed-shape.md)).
> The downgrade machinery is gone, and `auto` now prefers CoreML for
> `ohara index` on a CoreML-capable build (queries always embed on
> CPU). First use downloads the fp32 model (~130MB) and each indexing
> run pays a one-time ~30s CoreML compile. Indexes built with either
> provider share one vector space — no rebuild when switching.

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
