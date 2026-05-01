# `ohara init`

Install the post-commit hook (and optionally a CLAUDE.md stanza) so a
repo stays auto-indexed after every commit.

The hook is wrapped in fence comments
(`# >>> ohara managed (do not edit) >>>` … `# <<< ohara managed <<<`),
so re-running `ohara init` is idempotent — your existing post-commit
content is preserved. The hook fails-closed if `ohara` is not on
`PATH`, so it never blocks a commit.

## Usage

```
ohara init [PATH] [--write-claude-md] [--force]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` (positional) | `.` | Path to the repo (or any path inside it — `git2::Repository::discover` resolves the actual `.git` dir). |
| `--write-claude-md` | off | Also append/update an "ohara" stanza in `CLAUDE.md` at the repo root, fenced by `<!-- ohara:start -->` … `<!-- ohara:end -->`. |
| `--force` | off | Overwrite an existing `post-commit` hook even if it lacks the ohara marker fences. Use with care — replaces the whole file. |

## Examples

Install the hook in the current repo:

```sh
ohara init
```

Install the hook *and* document the tool for Claude Code in
`CLAUDE.md`:

```sh
ohara init --write-claude-md
```

Install the hook in a specific repo, replacing any existing
`post-commit` content:

```sh
ohara init ~/code/some-repo --force
```

## What gets written

The managed `post-commit` block runs an incremental re-index in the
repo root and silently no-ops if `ohara` is missing:

```sh
# >>> ohara managed (do not edit) >>>
# Re-index this repo on every commit. Silently skipped if `ohara` is not on PATH.
if command -v ohara >/dev/null 2>&1; then
  ( cd "$(git rev-parse --show-toplevel)" && ohara index --incremental >/dev/null 2>&1 ) || true
fi
# <<< ohara managed <<<
```

`--write-claude-md` adds a short stanza pointing collaborators at
`find_pattern` and explaining how the index stays fresh. Three cases:
file missing → write a fresh `CLAUDE.md`; file present with markers →
replace the stanza in place; file present without markers → append the
stanza separated by a blank line.
