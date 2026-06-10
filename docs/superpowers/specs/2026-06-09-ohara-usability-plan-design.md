# ohara usability plan — adoption polish after the memory fix

Date: 2026-06-09
Status: approved (brainstorm 2026-06-09)
Related: 2026-06-09-ohara-daemon-consolidation-design.md (wave 1),
issues #61 (perf roadmap), #59 (HNSW), #55 (quantized reranker)

## Problem

ohara works, but daily use and adoption hit four friction points:

1. **Stale plugin version.** The Claude Code plugin wrapper hardcodes
   `OHARA_VERSION = '0.7.4'` (`plugins/ohara/bin/ohara-mcp`) while the
   repo ships 0.9.0. Plugin users silently miss six releases of fixes —
   including the quantized embedder that cuts memory substantially.
2. **Indexing a large repo is painfully slow.** The questdb test repo
   (~5,875-commit pinned fixture) is the canonical example. Plan-27
   (chunk-embed cache) and plan-28 (parallel pipeline, `--workers`)
   landed, but `docs/perf/v0.6-baseline.md` per-phase numbers were
   never refreshed — we do not currently know the dominant phase on
   0.9.x.
3. **Upgrades strand the index.** 0.9.0 changed the embedder; existing
   indexes report `needs rebuild` and `find_pattern` refuses to run
   until the user manually discovers and runs
   `ohara index --rebuild --yes`.
4. **Setup drift is invisible.** Duplicate MCP registrations, stale
   plugin binaries, incompatible indexes, and orphan daemons produce no
   diagnostics — the observed dev machine ran two full engines
   (~2.3 GB) without any signal.

Explicitly deprioritized for now (user decision, 2026-06-09):
zero-config auto-indexing on first `find_pattern` against an unindexed
repo. The bootstrap stays a manual `ohara index`.

## Workstreams

### B1 — plugin auto-version (small, immediate)

- The wrapper reads its version from the plugin's
  `.claude-plugin/plugin.json` at runtime instead of a hardcoded
  constant. `OHARA_PLUGIN_VERSION` stays as an override.
- The release flow bumps `plugin.json` alongside the crate version so
  there is a single source of truth (add to the release checklist /
  automation that produces `chore(release): X.Y.Z`).
- The wrapper sweeps old `~/.cache/ohara-plugin/v*` dirs after a
  successful new-version download (disk hygiene).

### B2 — indexing speed on questdb-class repos (measure first)

Step 1 — refresh the baseline. Run `tests/perf/quest_db_baseline.rs`
on current main, fill the per-phase wall-time table in `docs/perf/`
(new file for 0.9.x; the v0.6 doc's TODOs are obsolete). No
intervention ships before this lands.

Step 2 — attack the dominant phase, using the existing decision tree:
embed-dominant → execution-provider work; storage-dominant →
bulk-load; sequential phase shape → pipeline overlap (plan-28 already
addressed this).

Candidate interventions, ranked by expected win (pruned/re-ranked by
the baseline):

1. **Hardware EP without build-from-source.** Released binaries are
   CPU-only; CoreML/CUDA builds are 3-5x on embed but gated behind
   `cargo build --features …`. Feasibility spike: a
   `coreml`-featured `aarch64-apple-darwin` release artifact (the
   ort 2.0 xcframework is Apple-Silicon-only, which already dropped
   Intel) and/or runtime EP selection in one artifact.
2. **Promote `ohara plan` into the default flow.** `ohara index` on a
   repo above a commit threshold with no `.oharaignore` prints the
   hotmap hint up front, not as a docs-only feature.
3. **Depth-capped first index.** Apply the existing `--max-commits`
   design (2026-05-04 spec): index recent history first so the repo is
   queryable in minutes; optionally backfill older history afterwards.
4. **Warm-daemon embeds for incremental runs** — phase A4 of the
   daemon consolidation spec; removes the per-hook model load.

Exit criteria are set after Step 1 (a concrete "questdb fixture full
index in ≤ N minutes on M-series / ≤ M minutes CPU-only Linux"), and
the harness re-run is the acceptance gate for each intervention.

### B3 — self-healing upgrades (consent-gated, no silent rebuilds)

- `find_pattern`'s MCP error/hint becomes **structured and
  actionable**: `_meta.index = { compatibility: "needs_rebuild",
  command: "ohara index --rebuild --yes <repo>" }` (wording stays
  centralised in `ohara_core::index_metadata::compose_hint`). The tool
  description and the `ohara:indexing` skill document the agent flow:
  surface the command, run it with user consent.
- `ohara index` on a needs-rebuild index prompts to rebuild when
  attached to a tty; `--yes` covers non-interactive/agent use (current
  behavior, now documented as the path rather than a dead end).
- `ohara update` finishes by running the compatibility check and
  printing the exact recovery command when one is needed.
- Explicit non-goal: background auto-rebuild without consent.

### B4 — onboarding, docs, and `ohara doctor`

- New `ohara doctor` subcommand; each check prints a finding plus the
  exact fix command:
  - duplicate ohara MCP registrations (plugin + manual entries across
    `~/.claude.json`, `claude_desktop_config.json`, repo `.mcp.json`);
  - plugin cache binary version vs CLI version vs latest release;
  - per-repo index compatibility verdict (same logic as `ohara
    status`);
  - daemon registry health (alive / stale / orphaned versions);
  - model cache presence; glibc baseline on Linux.
- Docs: tighten the quickstart (the happy path is
  install → `ohara index` → plugin install, in that order), refresh the
  MCP-clients page, and add a troubleshooting page generated from the
  doctor checks.

## Sequencing

- **Wave 1 (memory first):** daemon consolidation A1-A3 (other spec),
  plus B1 (independent, small) and B2 Step 1 (baseline run —
  independent, informs everything).
- **Wave 2 (speed + upgrades):** B2 interventions chosen by the
  baseline, A4, B3.
- **Wave 3 (adoption):** B4.

## Risks / open questions

- ort 2.0 prebuilt EP artifacts: feasibility of a CoreML-featured
  release artifact is unproven (the spike in B2.1 resolves this before
  any commitment).
- Plugin marketplace update mechanics (how `/plugin` surfaces new
  versions) need verifying as part of B1.
