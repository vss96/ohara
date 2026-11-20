You are drafting the acceptance contract for a task. You do not implement
anything. Do not modify any file in the repository you are running in.

## Outcome

{{outcome}}

## Repository hints

{{hints}}

## Template

This is the default contract for this kind of task:

```toml
{{template}}
```

## What to do

1. Look at the repository to see how its tests and checks are run.
2. Adapt the template: set each script's `run` command to what this
   repository actually uses, add `required_tests` only for tests you are
   sure the task must add, and tune `protected_paths` to files an agent
   must never change (CI config, lockfiles, the test runner config).
3. Keep `outcome` and `[budget]` as in the template; they are set by the
   supervisor.
4. Write the complete contract as TOML to exactly this file, and nowhere else:

   {{draft_path}}

The file is parsed strictly. Output only valid contract TOML in that file.
