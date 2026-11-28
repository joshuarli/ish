#!/bin/sh
set -eu

runs=$(gh run list --repo "$GITHUB_REPOSITORY" --commit "$SHA" --limit 100 --json name,status,conclusion)
verdict=$(jq -rn --argjson runs "$runs" --arg workflow "$WORKFLOW" '
  $runs | map(select(.name != $workflow)) as $ci
  | if ($ci | length) == 0 then "no CI runs on this commit"
    elif ($ci | any(.status != "completed" or (.conclusion != "success" and .conclusion != "skipped" and .conclusion != "neutral"))) then "red or incomplete CI"
    else "green" end')
printf '%s\n' "$verdict"
test "$verdict" = green
