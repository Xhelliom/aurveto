# aurveto

A security guardrail for AUR updates. Born after the mass AUR compromise of
June 2026: rather than blindly installing the latest version of an AUR package,
`aurveto` applies a decision chain before every update.

> 🌐 **First visit? Start with the overview page:
> [xhelliom.github.io/aurveto](https://xhelliom.github.io/aurveto/)** — an
> illustrated overview of the project, its interfaces, and its philosophy.
> The rest of this README is the technical documentation.

## Decision chain

For each AUR package with an available update:

1. **Whitelist** — trusted packages (signed binaries from reputable vendors):
   the delay is skipped, but the static scan and AI review still apply.
2. **Delay** — two semantics (`delay_mode`):
   - **`lag`** (default): installs the PKGBUILD revision that was the `HEAD`
     of the AUR git repository `delay_days` days ago (this revision has been
     exposed to the community that whole time). Updates always arrive, with a
     constant lag — no permanent blocking of frequently updated packages.
     Since the AUR stores no binaries, the revision is **built locally**
     (`git checkout <commit>` + `makepkg -si`).
   - **`hold`**: blocks any update whose latest version is less than
     `delay_days` days old; stays on the installed version (stricter, but a
     package updated more often than the delay is never installed).
3. **Anti-revert guard** (lag mode) — a tainted version stays in the git history
   even after an in-place fix. We therefore refuse a target revision if it has
   been **reverted/cleaned up since**: either a later commit mentions a
   compromise, or a dangerous execution pattern (`| bash`, `base64 -d`,
   `/dev/tcp/`…) present in the target has disappeared from the current `HEAD`.
4. **Static scan** — delegates to [`aur-scan`](https://github.com/KiefStudioMA/ks-aur-scanner)
   if installed (70+ rules, IOC database). A blocking detection → refusal.
5. **AI review** — sends the PKGBUILD *diff* to an LLM (Groq / OpenAI /
   Anthropic, or a **local model** served by llama.cpp — configurable) which
   judges it `safe / suspect` with justification.

Only packages that pass all four steps are offered for installation.

## AI multi-vote (cost savings)

The AI review calls the model **only once** when the package is judged safe
(the common case). A block triggers additional votes (up to `confirm_votes`
total) and is confirmed only by a **strict majority** — which neutralizes false
positives caused by the model's non-determinism.

## Interfaces

Three frontends share the same core:

- **CLI** — `aurveto <command>`
- **TUI** (terminal, ratatui) — `aurveto config-ui`
- **GUI** (GTK4 / libadwaita) — `aurveto-gui` binary: editable settings +
  update report (✅ safe / ⏳ delayed / ⛔ blocked) + installation.

aurveto **only handles AUR packages** (the unverified content). The official
Arch repositories are signed and out of its scope. To avoid updating them
separately (and bypassing the AUR review with a `yay -Syu`), the `upgrade`
command chains both: `pacman -Syu` then the safe AUR packages.

```bash
aurveto            # report (alias of `check`), installs nothing
aurveto check      # same (+ reminder of the number of official updates)
aurveto upgrade    # official repos (pacman -Syu) THEN safe AUR packages
aurveto apply      # only the AUR packages judged safe
aurveto apply --dry-run
aurveto status     # age (last AUR change) of all installed AUR packages
aurveto config     # path + summary of the configuration
aurveto config-ui  # terminal settings interface (TUI)
aurveto install    # desktop entry + icon + translations + notification timer
aurveto review-file <PKGBUILD>  # (debug) AI review of a file
```

## Configuration

`~/.config/aurveto/config.toml` (created on first launch):

```toml
delay_days = 14
delay_mode = "lag"     # lag | hold
helper = "yay"
use_aur_scan = true
whitelist = ["google-chrome", "zen-browser-bin", "..."]

[ai]
enabled = true
provider = "groq"      # groq | anthropic | openai | local
model = ""              # empty => provider's default model
api_key_env = ""        # empty => GROQ_API_KEY / ANTHROPIC_API_KEY / OPENAI_API_KEY
local_endpoint = ""     # provider = "local" only; empty => http://127.0.0.1:8080/v1/chat/completions

[notify]
enabled = false             # systemd --user timer for desktop notifications
interval_hours = 6          # check frequency
silent_when_up_to_date = true
```

### Local model (llama.cpp)

`provider = "local"` sends the review to an OpenAI-compatible server running on
your machine — **no request leaves the host**, and no API key or account is
needed:

```bash
# The Arch package is spelled with a dash. ggml-cpu is required even when
# offloading to a GPU; add the backend for yours (ggml-cuda for NVIDIA,
# ggml-vulkan / ggml-hip otherwise).
pacman -S llama-cpp ggml-cpu ggml-cuda

llama-server -m ~/models/Qwen2.5-Coder-7B-Instruct-Q4_K_M.gguf \
  --port 8080 --n-gpu-layers 99
```

`llama-cpp` is **not installed automatically**: the GUI shows a banner with a
one-click install when `llama-server` is missing, and a reminder to start it
when the binary is there but nothing answers. `aurveto config` reports the same
state on the CLI.

Then set the provider to *Local (llama.cpp)* in the GUI/TUI, or in
`config.toml`. `local_endpoint` overrides the URL if the server listens
elsewhere; `model` is ignored by llama.cpp (it serves the model it was started
with) but is honoured by other OpenAI-compatible runtimes. If the server was
started with `--api-key`, store that key like any other one (env
`AURVETO_LOCAL_API_KEY` or `secrets.toml`). Switching back to a cloud provider
only changes `provider`; nothing else has to be undone.

**Vote policy differs on purpose.** With a cloud provider each call is billed,
so a `safe` verdict is taken on the first call and only a *block* is put to the
vote (majority confirms). A local runtime costs nothing per call, and the risk
profile is reversed — a smaller model missing a real compromise is worse than a
false alarm. So **every** verdict is voted on and `safe` requires
**unanimity**: one dissenting vote out of `confirm_votes` blocks the update.

### Measuring a model before trusting it

Model quality is the open question, not the runtime. `tests/bench-model.sh`
scores a model against the labelled PKGBUILD diffs in `tests/fixtures/`
(`safe-*` must be allowed, `unsafe-*` must be blocked):

```bash
cargo build
tests/bench-model.sh                                    # local, default endpoint
tests/bench-model.sh http://127.0.0.1:11434/v1/chat/completions qwen3-coder:7b
PROVIDER=groq tests/bench-model.sh                      # cloud baseline
```

Measured on an RTX PRO 2000 (8 GB) with **Qwen2.5-Coder-7B-Instruct Q4_K_M**,
13 fixtures, three consecutive runs:

| votes | false negatives | false positives | wall clock |
|-------|-----------------|-----------------|------------|
| 1     | **0**           | 2               | ~40 s (≈3 s/package)  |
| 3 (unanimity) | **0**   | 2               | ~125 s (≈10 s/package) |

Identical verdicts on all three runs: at `temperature = 0` this model is
deterministic, so the extra votes cost 3× the time and change nothing *here*.
They remain the insurance against a noisier model, not a measured gain on this
one.

Both false positives are on deliberately ambiguous fixtures, and one of them
exposes a real 7B weakness: on `safe-cdn-migration` the model's own summary
says *"a CDN is a common practice […] indicating a version bump"* — correct
reasoning — yet it still returns `safe: false`. A small model does not reliably
align its boolean with its analysis. Worth trying `response_format` with a JSON
schema before blaming the prompt.

It exits non-zero on any **false negative** — an `unsafe-*` fixture judged
safe — because that is the failure that would install a compromise. False
positives are only reported, they cost friction rather than security. The run
uses a throwaway config directory and never touches `~/.config/aurveto`.

The API key is **never** stored in `config.toml`. It is resolved from the
provider's environment variable first, otherwise from a dedicated file
`~/.config/aurveto/secrets.toml` (permissions `0600`), which can be filled in
from the interfaces (GUI/TUI).

## Settings interfaces

The GUI puts **updates on the home page** and groups the settings into a
separate **full-screen page** (gear button → navigation): delay/mode/helper/scan,
AI review (provider, **model**, **local endpoint**, **API key**, votes), the **whitelist** (editing +
suggestions from installed AUR packages), and **notifications** (enabling,
interval). The TUI (`aurveto config-ui`) offers the same settings via keyboard.

## Desktop integration and notifications

`aurveto install` installs the menu entry (`.desktop`), the icon, and the
translations, then sets up a systemd `--user` timer
(`aurveto-notify.timer`) that periodically runs `aurveto notify`: it
**counts** the available official and AUR updates (without scan or AI review, so
without API cost) and sends a notification via `notify-send`. Enabling and the
interval are set from the GUI/TUI or the `[notify]` section of `config.toml`;
any save of the settings re-syncs the timer.

## Languages

The interface (CLI, TUI, GUI) is multilingual via gettext and follows the
**system locale**. English by default, French provided. To install the
translations:

```bash
po/install.sh            # compiles po/*.po → ~/.local/share/locale/<lang>/…
```

## Installation

### From the AUR (recommended)

`aurveto` is published on the AUR in two flavours; both pull in `gtk4` /
`libadwaita` automatically and install the binaries, the desktop entry, the
icon and the translations:

```bash
yay -S aurveto-bin   # precompiled binaries (x86_64, fastest)
yay -S aurveto       # builds from source
```

Optionally install [`aur-scan`](https://github.com/KiefStudioMA/ks-aur-scanner)
to enable the static-analysis layer.

### From source

```bash
# CLI + TUI + GUI (default; requires gtk4 and libadwaita ≥ 1.4)
cargo build --release

# All-in-one: copies the binaries (~/.local/bin), installs the menu entry +
# icon (Exec as an absolute path), installs the translations and the
# notification timer. Run from the built tree:
./target/release/aurveto install

# Variant without GUI (headless machine / CLI only):
cargo build --release --no-default-features --features tui
```

> Without the GUI (`--no-default-features`), only the CLI binary is copied and
> the menu entry is skipped (the shortcut would point nowhere).

## Limitations

- The delay also delays legitimate security fixes → hence the whitelist for
  trusted packages.
- A compromise undetected for longer than `delay_days` slips through the delay
  (but not necessarily through the scan / AI review).
- The AI review depends on the model's quality; it complements, not replaces,
  human reading of the diff.
