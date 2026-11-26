#!/bin/sh
# Claude Code `Notification` hook: shows Claude's notification text as an
# aiterm desktop notification (OSC 777, shown while the window is unfocused).
#
# Claude Code writes the returned `terminalSequence` to the terminal itself;
# see https://code.claude.com/docs/en/hooks. Requires jq.
input=$(cat)
# Replace control characters so the text can't end or inject escape sequences;
# `;` separates the OSC 777 title from the body, so remove it from the title.
title=$(printf '%s' "$input" | jq -j '.title // "Claude Code"' | tr -s '\000-\037\177;' ' ')
body=$(printf '%s' "$input" | jq -j '.message // "Needs your attention"' | tr -s '\000-\037\177' ' ')
seq=$(printf '\033]777;notify;%s;%s\033\\' "$title" "$body")
jq -nc --arg seq "$seq" '{terminalSequence: $seq}'
