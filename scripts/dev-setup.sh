#!/usr/bin/env bash
# What a new machine needs before `just parity` can run, printed as a checklist.
#
# The Obsidian CLI cannot register a vault: `obsidian help` offers `vault`,
# `vaults` and `open`, and nothing that adds one to the registry. So the last
# step is a manual one, and this script's job is to make it obvious rather than
# to hide it behind a flag that does nothing.
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vault_path="$repo_root/test/vault"

echo "Obsidian CLI gates"
echo "==================="
echo

if command -v obsidian >/dev/null 2>&1; then
	echo "  CLI binary    $(command -v obsidian)"
	echo "  CLI version   $(obsidian version 2>/dev/null || echo 'unavailable')"
else
	echo "  CLI binary    NOT FOUND"
	echo
	echo "  The CLI ships with the Obsidian installer, not the app. Install the"
	echo "  1.12+ installer, then re-run this script."
	exit 1
fi

echo "  vaults        $(obsidian vaults 2>/dev/null | tr '\n' ' ' || echo 'unavailable')"
echo

if obsidian vaults 2>/dev/null | grep -qxF vault; then
	echo "  The testing vault is registered as 'vault'."
else
	echo "  The testing vault is NOT registered. Add it in Obsidian:"
	echo
	echo "      Settings -> Vaults -> Manage vaults -> Open folder as vault"
	echo "      $vault_path"
	echo
	echo "  Then re-run this script to confirm."
fi

echo
echo "Other configuration"
echo "==================="
echo
echo "  Copy .env.example to .env and edit it for anything machine-specific."
echo "  .env is gitignored; .env.example is committed."
