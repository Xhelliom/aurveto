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
   Anthropic, configurable) which judges it `safe / suspect` with justification.

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
provider = "groq"      # groq | anthropic | openai
model = ""              # empty => provider's default model
api_key_env = ""        # empty => GROQ_API_KEY / ANTHROPIC_API_KEY / OPENAI_API_KEY

[notify]
enabled = false             # systemd --user timer for desktop notifications
interval_hours = 6          # check frequency
silent_when_up_to_date = true
```

The API key is **never** stored in `config.toml`. It is resolved from the
provider's environment variable first, otherwise from a dedicated file
`~/.config/aurveto/secrets.toml` (permissions `0600`), which can be filled in
from the interfaces (GUI/TUI).

## Settings interfaces

The GUI puts **updates on the home page** and groups the settings into a
separate **full-screen page** (gear button → navigation): delay/mode/helper/scan,
AI review (provider, **model**, **API key**, votes), the **whitelist** (editing +
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

## Build

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
