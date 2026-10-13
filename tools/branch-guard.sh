#!/usr/bin/env bash
# Refuses a commit made directly on main or dev (see AGENTS.md, Git and pull requests).
case "$1" in
  main|dev) echo "Never commit on $1 directly: create a branch first." >&2; exit 1 ;;
esac
