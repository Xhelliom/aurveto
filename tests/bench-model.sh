#!/usr/bin/env bash
# Measures how well a model judges the fixtures in tests/fixtures/.
#
# Each fixture encodes its expected verdict in its name: `safe-*` must be
# allowed, `unsafe-*` must be blocked. A false NEGATIVE (an unsafe diff judged
# safe) is the one that matters: it means the chain would install a compromise.
#
# Usage:
#   tests/bench-model.sh                          # local runtime, defaults
#   tests/bench-model.sh <endpoint> [model]       # a specific local server
#   PROVIDER=groq tests/bench-model.sh            # compare against the cloud
#
# Runs against a throwaway XDG_CONFIG_HOME: your real config is never touched.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
bin="$root/target/debug/aurveto"
[ -x "$bin" ] || bin="$root/target/release/aurveto"
[ -x "$bin" ] || { echo "build aurveto first (cargo build)"; exit 1; }

endpoint="${1:-http://127.0.0.1:8080/v1/chat/completions}"
model="${2:-}"
provider="${PROVIDER:-local}"
# One vote: we measure the model itself, not the vote policy on top of it.
votes="${VOTES:-1}"

cfgdir="$(mktemp -d)"
trap 'rm -rf "$cfgdir"' EXIT
mkdir -p "$cfgdir/aurveto"
cat > "$cfgdir/aurveto/config.toml" <<EOF
delay_days = 14
delay_mode = "lag"
helper = "yay"
use_aur_scan = false
whitelist = []

[ai]
enabled = true
provider = "$provider"
model = "$model"
api_key_env = ""
local_endpoint = "$endpoint"
confirm_votes = $votes

[notify]
enabled = false
interval_hours = 6
silent_when_up_to_date = true
EOF

echo "provider=$provider model=${model:-<default>} endpoint=$endpoint votes=$votes"
printf '%-28s %-10s %-10s %s\n' FIXTURE EXPECTED GOT RESULT
echo "--------------------------------------------------------------------"

fn=0; fp=0; ok=0; err=0; t0=$SECONDS
for f in "$here"/fixtures/*.diff; do
  name="$(basename "$f" .diff)"
  case "$name" in safe-*) want=safe ;; *) want=blocked ;; esac

  # LC_ALL=C keeps the output in the untranslated source language.
  out="$(LC_ALL=C LANGUAGE= XDG_CONFIG_HOME="$cfgdir" "$bin" review-file "$f" 2>&1)"
  verdict="$(printf '%s' "$out" | sed -n 's/^ *safe *: *//p')"
  case "$verdict" in
    true)  got=safe ;;
    false) got=blocked ;;
    *)     got=error ;;
  esac

  if [ "$got" = error ]; then
    res="ERROR"; err=$((err+1))
    printf '%-28s %-10s %-10s %s\n' "$name" "$want" "$got" "$res"
    printf '%s\n' "$out" | sed 's/^/      /' | tail -3
    continue
  elif [ "$got" = "$want" ]; then
    res="ok"; ok=$((ok+1))
  elif [ "$want" = blocked ]; then
    res="** FALSE NEGATIVE **"; fn=$((fn+1))
  else
    res="false positive"; fp=$((fp+1))
  fi
  printf '%-28s %-10s %-10s %s\n' "$name" "$want" "$got" "$res"
done

echo "--------------------------------------------------------------------"
echo "ok=$ok  false-negatives=$fn  false-positives=$fp  errors=$err  ($((SECONDS-t0))s)"
# A single missed compromise disqualifies the model for this job.
[ "$fn" -eq 0 ] && [ "$err" -eq 0 ]
