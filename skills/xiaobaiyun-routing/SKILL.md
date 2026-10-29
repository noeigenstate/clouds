---
name: xiaobaiyun-routing
description: How the global model-routing pipeline (Opus plans, GPT-6.1-Sol and Sonnet build in worktrees, the lead integrates, Opus + GPT-6.1-Terra review) applies to this Rust + Slint project - who owns which files, the checks, worktree builds that don't lock the owner's running exe, and what workers must never do here. Use together with model-routing at the start of every task that changes code, docs or UI in this repo.
---

# Model routing in 小白云

The pipeline, roles and model settings come from the global `model-routing` skill. This file only adds what is
specific to this repo.

## Who takes what here

| Work | Goes to |
| --- | --- |
| `selection.rs`, `ocr.rs`, `translate.rs`, `update.rs`, `cloud.rs`, `window.rs`: logic, Win32, debugging, failing tests | GPT-6.1-Sol (2A) |
| `ui/*.slint` layout and polish, `settings_window.rs`, wording, README, skill docs, routine unit tests | Sonnet (2B) |
| A hard animation or state change inside `hud.slint` | Sol |
| Reading screenshots of the cloud or the settings window, one-line fixes | GPT-6.1-Terra |

## Briefs

Every brief:
- points to `skills/slint-floating-hud/SKILL.md` (house rules, how to verify) and `rustfmt.toml`;
- says that clean `cargo fmt --check`, `cargo clippy --release --all-targets` and `cargo test --release` are
  required before the worker reports;
- forbids running `xiaobaiyun.exe` or `cargo run`, the `#[ignore]` tests (they read the real screen), sending
  keys or mouse input, and touching `%APPDATA%\xiaobaiyun`. A running copy hooks the owner's mouse and
  translates their real selections.

Codex has no GUI here anyway; the lead runs test copies in stage 3.

Paste the `debug.log` lines a bug needs into the brief instead of pointing a worker at the log: it holds the
owner's real selections.

## Worktrees and builds

- Always build in a worktree, even for a one-sided task. The owner's cloud often runs from the repo's
  `target\release\xiaobaiyun.exe`, and a build that has to replace the running exe fails.
- A fresh worktree compiles Slint from scratch (about 5 minutes for a release build). Copying the repo's
  `target` into it first is slower than that: don't.
- Put worktrees in the scratchpad (`<scratch>/wt-a`). Remove them with `git worktree remove --force`, then delete
  the branch.

## Stage 3 and 4 here

- Stage 3 runs the three cargo checks and a release build in the merged worktree. Then it checks behaviour with
  test copies the way `slint-floating-hud` describes: `--translate`, moved aside, DPI-aware screenshots,
  `debug.log`, every copy stopped afterwards.
- Terra's pass is only for changes that show on screen. Logic-only fixes such as selection or OCR get the Opus
  review only.
- Releases stay with the lead, and only on the owner's request.
