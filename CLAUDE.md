# CLAUDE.md — aurveto development guide

Instructions for anyone (or any agent) modifying this repository. Read before
writing code. The rules below **take precedence** over default habits.

## The project in one sentence

A security guardrail for AUR updates: for each package, a decision chain
**whitelist → delay (lag/hold) → anti-revert guard → static scan →
AI review** decides whether to install, delay, or block.

## Architecture

A library core (`src/lib.rs`, crate `aurveto`), shared by three
frontends. **All business logic lives in the lib; the frontends only
present and delegate.**

| Module            | Single responsibility |
|-------------------|-----------------------|
| `config`          | Config schema, defaults, TOML (de)serialization |
| `aur`             | All AUR I/O: RPC, git, PKGBUILD, vercmp. **No other module speaks HTTP/git.** |
| `scan`            | Delegation to `aur-scan` (static analysis) |
| `ai`              | Multi-provider, multi-vote AI review |
| `pipeline`        | Orchestration of the decision chain (little direct I/O, calls the others) |
| `main` (bin)      | CLI |
| `tui` (feat `tui`)| Terminal interface (ratatui) |
| `bin/gui` (feat `gui`) | GTK4/libadwaita interface |

Boundaries: an implementation detail (AUR URL, git command, a provider's JSON
schema) must **not** leak outside its module. If `pipeline` needs a piece of
AUR data, it calls a function in `aur`; it does not build a URL itself.

## Commands

```bash
cargo build                                       # CLI + TUI + GUI (default, gtk4 + libadwaita >= 1.4)
cargo build --no-default-features --features tui  # CLI + TUI, without GUI
cargo build --no-default-features                 # core + CLI only (must compile)
cargo fmt
cargo clippy --all-targets
```

## Code rules (mandatory)

### Quality — the gate before every commit
- `cargo fmt` applied, `cargo clippy --all-targets` **with zero warnings**, and
  the three feature combinations above all compile. A commit that is not
  fmt+clippy clean is not ready.

### DRY — don't write the same logic twice
- Factor out at the **rule of three** (3rd duplication). Existing examples to
  reuse/follow: `pipeline::vet()` (the scan+AI step shared by both paths),
  `pipeline::outcome()` (building an `Outcome`), `aur::run_git()`.
- A duplication of *structure* (the same struct literal repeated, the same
  scan→block→AI sequence) is a signal to refactor, not a style.

### Zero magic numbers / magic strings
- Every meaningful literal value becomes a **named constant**,
  close to its use site, documented. Existing models: `aur::SECS_PER_DAY`,
  `aur::RPC_BATCH`, `aur::AUR_HOST`, `aur::USER_AGENT`, `aur::DYNAMIC_VERSION`,
  `ai::MAX_TOKENS`, `ai::TEMPERATURE`, `main::SHORT_HASH_LEN`.
- Forbidden: `86_400`, `"https://aur.archlinux.org/..."`, `512`, `7` written
  inline in the logic. URLs, headers, batch sizes, thresholds → constants.
- A sentinel string (e.g. `"?"` for a dynamic version) is a shared constant,
  never compared against a literal far from its definition.

### Error handling
- `anyhow::Result` + `.context(...)` to pinpoint the failure. Propagate with `?`.
- **No `unwrap()`/`expect()`/`panic!` in the lib.** Tolerated only in the
  frontends for a truly impossible invariant, and then commented.
- A network/git/process operation that fails must never crash an
  evaluation: log it (`eprintln!`) and fall back to a safe decision.

### Security — non-negotiable invariants (this is a security tool)
- **Fail-closed**: any doubt (error, missing info, unknown version,
  ambiguity) leans toward *delay/block*, never toward *install*.
- The scan and the AI review apply to **exactly the revision that will be
  installed** (in lag mode: the lagged revision, not the latest).
- Secrets (API keys) never go into `config.toml`, the logs, or an
  error message. Resolution: environment variable first, otherwise the
  dedicated `secrets.toml` file written with mode `0600` (`config::Secrets` /
  `config::resolve_api_key`). A UI never pre-fills an existing key.
- Don't widen an "allowed" decision unless the upstream steps have
  validated it.

### Functions and readability
- One function = one responsibility. Early returns rather than deep
  nesting. If a function exceeds ~50 lines or nests > 3 levels, split it.
- Explicit names (identifiers and names should be in English, the repo's source
  language: `lagged_target`, `reverted_since`), `snake_case`, no obscure
  abbreviations.

### Comments
- Doc comments `///` on **every public item**, describing the contract.
- Comments explain the **why / intent / a constraint**,
  not a paraphrase of the code. No comment that repeats the next line.

### Frontends
- `main`, `tui`, `gui` contain **no** business logic: they read the
  config, call `pipeline`/`aur`, and display. Every decision rule is
  coded (and tested) in the lib.
- The `tui` and `gui` features stay optional; the core compiles without them.

### Tests
- Pure functions (parsing, comparisons, heuristics like
  `danger_signatures`) deserve `#[cfg(test)]` tests.
- Side-effecting flows (git, AI) are exercised via the debug subcommands
  (`review-file`, `revert-check`) on controlled fixtures.

### Dependencies
- Minimal and justified. Pin major versions. Prefer a short in-house
  function (cf. `unified_diff`, `urlencode`) over a heavy dependency for a
  trivial need.

### Commits
- Imperative messages, prefixed `feat:`/`fix:`/`refactor:`/`docs:`/`chore:`,
  explaining the **why**. One commit = one coherent change.

## Internationalization (i18n)
- Every **user-facing** string (CLI, TUI, GUI) goes through the
  `t!(...)` macro (gettext). Source in **English** (msgid); translations in
  `po/<lang>.po`. Positional interpolation with `{}`: `t!("Found {}", n)`.
- After adding/changing a `t!` string, update `po/fr.po` then recompile
  the `.mo` files via `po/install.sh`.
- Internal `.context()`/`bail!` calls and code comments stay in
  English (the repo's source language), untranslated.
