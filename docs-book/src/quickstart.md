# Quickstart

Five minutes from `curl | sh` to "Claude can ask the past how I solved
this before." Assumes the [install](./install.md) is done and both
`ohara` and `ohara-mcp` are on `PATH`.

## 1. Index a repo

Pick a real repo with some history (the demo is more compelling on
something with a few hundred commits than on a fresh `git init`):

```sh
cd ~/code/some-real-repo
ohara index
```

The first run downloads the BGE-small embedding model (~80 MB,
one-time) and then walks every commit, embedding diffs and extracting
HEAD-snapshot symbols. On a small repo this finishes in seconds; on a
QuestDB-class polyglot codebase it currently takes a while — see the
[v0.6 throughput RFC](https://github.com/vss96/ohara/blob/main/docs/superpowers/specs/2026-05-01-ohara-v0.6-indexing-throughput-rfc.md).

The index lives at `$OHARA_HOME/<repo-id>/index.sqlite` (defaults to
`~/.ohara/`). Nothing leaves your machine.

## 2. Install the post-commit hook

So the index stays fresh as you commit:

```sh
ohara init
```

This drops a managed block into `.git/hooks/post-commit` that runs
`ohara index --incremental` after every commit. The hook is wrapped in
fence comments (`# >>> ohara managed >>>` … `# <<< ohara managed <<<`),
so re-running `ohara init` is idempotent and your existing hook
content is preserved. The hook fails-closed if `ohara` isn't on
`PATH` — it never blocks a commit.

If you also want a CLAUDE.md stanza describing the tool:

```sh
ohara init --write-claude-md
```

See [`ohara init`](./cli/init.md) for the full flag set.

## 3. Run a query

Sanity-check the index from the CLI before wiring into Claude:

```sh
ohara query --query "retry with backoff" --k 3
```

The output is the same JSON envelope the MCP `find_pattern` tool
returns. Each hit carries the commit SHA, message, file path, a
truncated diff excerpt, similarity / recency / combined scores, and
provenance.

## 4. Wire into Claude Code

Point Claude at `ohara-mcp` and you're done — see
[Wiring into Claude Code](./claude-code.md) for the
`claude_desktop_config.json` snippet. Both
[`find_pattern`](./tools/find_pattern.md) and
[`explain_change`](./tools/explain_change.md) become available to
Claude in any session whose working directory matches an indexed
repo.

## What next?

- Learn what the MCP tools do: [`find_pattern`](./tools/find_pattern.md),
  [`explain_change`](./tools/explain_change.md).
- Check on an index: [`ohara status`](./cli/status.md).
- Understand what the index actually contains:
  [Architecture overview](./architecture/overview.md).
