# `ohara update`

Self-update the installed `ohara` binary by checking GitHub Releases
for a newer version and replacing the on-disk binary in place.

Backed by [`axoupdater`](https://github.com/axodotdev/axoupdater),
the same library that powers cargo-dist's standalone
`<app>-update` helper. `ohara update` and `ohara-cli-update` always
agree on what's latest because they read the same release manifest.

## Usage

```
ohara update [--check] [--force] [--prerelease]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--check` | off | Report whether a newer version exists without installing it. Exits **2** if an update is available, **0** if up-to-date. |
| `--force` | off | Allow downgrades / re-installs of the same version. Off by default — running `ohara update` on the latest version is a no-op. |
| `--prerelease` | off | Include pre-release tags when looking for "latest". Default is stable-only. |

## Examples

Install the latest stable release:

```sh
ohara update
```

Just check, don't install:

```sh
ohara update --check
```

Re-install the same version (e.g. after a corrupted binary):

```sh
ohara update --force
```

## Requirements

`ohara update` only works when the binary was installed via the
curl-pipe-sh installer described on the [Install](../install.md)
page. It locates the install receipt the installer dropped beside
the binary; without that receipt it fails with a clear error.

If you built from source or unpacked a tarball by hand, "update" by
re-running the installer or rebuilding from the tagged release.
