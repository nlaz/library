#!/bin/zsh
# Backfill data/titles.json from doc ids: download filenames carry the
# title inline, Internet Archive ids resolve via archive.org metadata,
# kebab ids prettify. Writes data/titles.json.proposed for human review —
# never overwrites titles.json. IA fetch misses become "FIXME …" entries
# (curate via the app's set_title).
#
#   tools/backfill-titles.sh [data-dir]      # default: data
set -euo pipefail

DATA="${1:-data}"
TEXT="$DATA/text"
EXISTING="$DATA/titles.json"
OUT="$DATA/titles.json.proposed"
[ -d "$TEXT" ] || { echo "no $TEXT directory" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq required" >&2; exit 1; }

existing="{}"
[ -f "$EXISTING" ] && existing=$(cat "$EXISTING")

# IA metadata is per-item; local -N suffixed copies share one item. Append
# volume/date when present so multi-volume copies stay tellable-apart.
ia_title() {
  local id="$1" base meta title vol date
  base="${id%%-<->}"; base="${id%-[0-9]*}"
  meta=$(curl -sf --max-time 15 "https://archive.org/metadata/$base" || true)
  [ -n "$meta" ] || { echo ""; return; }
  title=$(printf '%s' "$meta" | jq -r '.metadata.title // empty')
  [ -n "$title" ] || { echo ""; return; }
  vol=$(printf '%s' "$meta" | jq -r '.metadata.volume // empty')
  date=$(printf '%s' "$meta" | jq -r '.metadata.date // empty')
  if [ -n "$vol" ]; then echo "$title, $vol"
  elif [ -n "$date" ]; then echo "$title ($date)"
  else printf '%s\n' "$title"; fi
}

prettify() {
  printf '%s\n' "$1" | tr '-' ' ' | awk '{for(i=1;i<=NF;i++){w=$i; if(length(w)>2) w=toupper(substr(w,1,1)) substr(w,2); printf "%s%s", w, (i<NF?" ":"")}; print ""}'
}

# one {"stem":"title"} object per line; merged once at the end (robust to
# any control characters IA metadata sneaks into a title)
ENTRIES=$(mktemp)
trap 'rm -f "$ENTRIES"' EXIT
emit() { jq -cn --arg d "$1" --arg t "$2" '{($d):$t}' >> "$ENTRIES"; }

copy=0
for f in "$TEXT"/*.md; do
  stem="${f:t:r}"
  # already curated → keep as-is
  have=$(printf '%s' "$existing" | jq -r --arg d "$stem" '.[$d] // empty')
  if [ -n "$have" ]; then
    emit "$stem" "$have"
    continue
  fi
  case "$stem" in
    *" ("*)  # download filename: title precedes the first " ("
      title="${stem%% \(*}"
      ;;
    [a-z]*00[a-z][a-z][a-z][a-z]|[a-z]*00[a-z][a-z][a-z][a-z]-[0-9]*)  # IA id
      title=$(ia_title "$stem")
      if [ -z "$title" ]; then
        copy=$((copy+1))
        title="FIXME copy $copy ($stem)"
      elif [[ "$stem" == *-[0-9]* ]]; then
        # local -N copies share one IA item; keep them tellable-apart
        title="$title · copy ${stem##*-}"
      fi
      echo "  $stem → $title" >&2
      ;;
    *)
      title=$(prettify "$stem")
      ;;
  esac
  # squeeze runs of whitespace/control chars IA titles sometimes carry
  title=$(printf '%s\n' "$title" | tr -s '[:space:][:cntrl:]' ' ' | sed 's/^ *//; s/ *$//')
  emit "$stem" "$title"
done

jq -s 'add' "$ENTRIES" > "$OUT"
echo "wrote $OUT ($(jq 'length' "$OUT") entries) — review, then: mv $OUT $EXISTING" >&2
