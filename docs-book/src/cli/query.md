# `ohara query`

Run a `find_pattern` query from the command line. Useful for
sanity-checking an index without going through an MCP client, and for
piping ranked hits into `jq` for ad-hoc analysis.

Returns the same JSON envelope as the MCP tool — see
[`find_pattern`](../tools/find_pattern.md) for the response shape.

## Usage

```
ohara query [PATH] --query <STRING> [--k N] [--language LANG] [--no-rerank]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` (positional) | `.` | Path to the repo. |
| `-q`, `--query` | required | Natural-language query string. |
| `-k`, `--k` | `5` | Number of results to return. |
| `--language` | `null` | Filter results to a single language (`rust`, `python`, `java`, `kotlin`). |
| `--no-rerank` | off | Skip the cross-encoder rerank stage. Faster, deterministic, slightly less precise on the top result. Skips the rerank model download too. |

## Examples

Top-5 retry-with-backoff matches in the current repo:

```sh
ohara query --query "retry with backoff"
```

Top-3 Rust-only matches, piped through `jq`:

```sh
ohara query --query "exponential retry" --k 3 --language rust | jq '.[].commit_message'
```

Skip the cross-encoder for a faster, deterministic ranking:

```sh
ohara query --query "auth middleware" --no-rerank
```

## Notes

- The `--since` filter exposed by the MCP tool (`since: "30d"`,
  `since: "2024-01-01"`) is not currently surfaced on the CLI.
- The CLI prints the `hits` array directly (no `_meta` envelope) so
  the output is a JSON array, not the `{ hits, _meta }` document the
  MCP tool returns.
