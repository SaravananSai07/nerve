#!/usr/bin/env bash
# Build research/sync/site/index.html: one self-contained, offline page
# with the report, decision log, experiment results, surveys, raw outputs
# and prototype scripts. No network, no CDN; open it with any browser.
#
#   research/sync/site/build.sh && open research/sync/site/index.html
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
sync="$(cd "$here/.." && pwd)"
repo="$(cd "$sync/../.." && pwd)"
out="$here/index.html"

# group|title|path (relative to repo)
docs=(
  "Overview|Guide (start here)|research/sync/GUIDE.html"
  "Overview|Round 1 summary|research/sync/site/summary.html"
  "Overview|Report|research/sync/REPORT.md"
  "Overview|Decision log|research/sync/DECISIONS.md"
  "Overview|Experiment results|research/sync/experiments/RESULTS.md"
  "Overview|User guide (docs/SYNC.md)|docs/SYNC.md"
  "Overview|Run the prototype|research/sync/demo/README.md"
  "Surveys|Nostr deep dive|research/sync/reports/01-nostr-deep-dive.md"
  "Surveys|48 alternatives|research/sync/reports/02-alternatives-survey.md"
  "Surveys|Claude Code control surfaces|research/sync/reports/03-claude-code-control-surfaces.md"
)
for f in "$sync"/experiments/*.out; do
  docs+=("Raw outputs|$(basename "$f")|${f#"$repo"/}")
done
for f in "$sync"/experiments/*.sh "$sync"/experiments/claude/*.sh "$sync"/demo/demo.sh; do
  docs+=("Experiment scripts|${f#"$sync"/}|${f#"$repo"/}")
done
for f in "$sync"/lab/src/*.rs; do
  docs+=("Prototype v1 lab (Rust)|$(basename "$f")|${f#"$repo"/}")
done
for f in "$repo"/src/sync/*.rs "$repo"/src/tui/devices.rs "$repo"/src/app/sync_ui.rs; do
  docs+=("nerve sync source|${f#"$repo"/src/}|${f#"$repo"/}")
done
for f in "$repo"/mod/nerve/hooks/register.js "$repo"/mod/nerve/.claude-plugin/plugin.json "$repo"/mod/.claude-plugin/marketplace.json; do
  docs+=("The nerve mod|${f#"$repo"/mod/}|${f#"$repo"/}")
done

{
  cat "$here/template-head.html"
  i=0
  for d in "${docs[@]}"; do
    IFS='|' read -r group title path <<<"$d"
    [ -f "$repo/$path" ] || { echo "missing: $path" >&2; exit 1; }
    if grep -qi '</script' "$repo/$path"; then
      echo "cannot embed (contains </script): $path" >&2; exit 1
    fi
    printf '<script type="text/plain" class="doc" id="d%d" data-group="%s" data-title="%s" data-path="%s">' \
      "$i" "$group" "$title" "$path"
    cat "$repo/$path"
    printf '</script>\n'
    i=$((i + 1))
  done
  printf '<p id="built">Built %s from branch %s (%s).</p>\n' \
    "$(date '+%Y-%m-%d %H:%M')" \
    "$(git -C "$repo" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')" \
    "$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo '?')"
  cat "$here/template-tail.html"
} >"$out"
echo "wrote $out ($i documents)"
