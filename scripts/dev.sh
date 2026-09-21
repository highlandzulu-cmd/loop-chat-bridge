#!/usr/bin/env bash
# One-command dev startup: builds and runs loop-server, then the web
# frontend, and tears both down together on Ctrl+C / exit.
#
# Usage:
#   ./scripts/dev.sh
#
# Configure via .env in the repo root (copy .env.example — see it for every
# available option and what it means). Missing .env is fine; loop-server
# just falls back to whatever's in ~/.loop/agent/settings.json.
#
# Run from anywhere; paths below are resolved relative to this script, not
# the caller's current directory.
set -euo pipefail
# Job control on, even though this runs non-interactively: it's what gives
# each `&` background job its own process group. Without it, `kill $pid` on
# the `npm run dev` job only kills that immediate subshell — npm's actual
# child (vite) is a grandchild in a different process and is left running,
# orphaned, still holding port 5173. Verified live: without `set -m`, Ctrl-C
# on this script stopped loop-server but left vite running in the
# background. With it, `kill -- -$pid` below (negative PID = whole group)
# takes the real process down too.
set -m

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# --- validation for the setup prompts below ---------------------------------
# The provider prompt used to accept any text into a hidden "Paste your API
# key" field. People pasted the whole config block (LOOP_SERVER_PROVIDER=...)
# there, it was saved as SOKET_API_KEY=LOOP_SERVER_PROVIDER=..., and because
# the field is hidden nobody saw it happen — the bridge then booted with no
# provider and a garbage key, and every message failed with a confusing 401.
# Every prompt now checks what it got and says why it refused.
looks_like_pasted_config() {
	# Whitespace, or a second NAME_LIKE_THIS= assignment, means this isn't a
	# single value at all.
	case "$1" in *[[:space:]]*) return 0 ;; esac
	printf '%s' "$1" | grep -Eq '[A-Z][A-Z0-9]*_[A-Z0-9_]+='
}
# A multi-line paste leaves its remaining lines in the terminal's input
# buffer, where the next prompt would silently consume them.
drain_stdin() { while IFS= read -r -t 1 -s _drain 2>/dev/null; do :; done; }

ask_secret() { # $1 = variable to set, $2 = prompt text
	local v
	while true; do
		read -r -s -p "$2" v
		echo
		if [ -z "$v" ]; then
			echo "    Nothing entered — try again (Ctrl+C to quit)."
			continue
		fi
		if looks_like_pasted_config "$v"; then
			drain_stdin
			echo "    That looks like config text (KEY=VALUE lines), not a key. Paste only the"
			echo "    key itself, not the variable name. Nothing was saved."
			continue
		fi
		if [ "${#v}" -lt 8 ]; then
			echo "    That's only ${#v} characters — too short to be an API key. Try again."
			continue
		fi
		echo "    Got ${#v} characters, starting with '${v:0:3}'."
		printf -v "$1" '%s' "$v"
		return 0
	done
}

ask_field() { # $1 = variable to set, $2 = prompt text, $3 = ERE it must match, $4 = what's expected
	local v
	while true; do
		read -r -p "$2" v
		if [ -n "$v" ] && printf '%s' "$v" | grep -Eq "$3"; then
			printf -v "$1" '%s' "$v"
			return 0
		fi
		drain_stdin
		echo "    Expected $4 — try again (Ctrl+C to quit)."
	done
}

if [ ! -f .env ]; then
	cp .env.example .env
	echo "==> Created .env from .env.example (first run)."
fi

# A missing/unconfigured model provider used to be the single most confusing
# failure mode here: the bridge would build fine, boot fine, log "harness
# ready", and then every real message would just silently fail — no error,
# no hint why. Catching it here, before anything starts, and actually
# asking for what's missing beats that every time.
#
# Heuristic, not exhaustive: if LOOP_SERVER_PROVIDER is already set in .env,
# assume a custom provider was deliberately configured (with its own key)
# and don't second-guess it. Otherwise, this is about to fall back to the
# harness's built-in "soket" default, which needs one of three possible
# key env vars — check for any of those before assuming nothing's set up.
if ! grep -qE '^LOOP_SERVER_PROVIDER=' .env 2>/dev/null; then
	if [ -z "${SOKET_API_KEY:-}${TENSORSTUDIO_API_KEY:-}${LOOP_API_KEY:-}" ] \
		&& ! grep -qE '^(SOKET_API_KEY|TENSORSTUDIO_API_KEY|LOOP_API_KEY)=' .env 2>/dev/null; then
		echo
		echo "==> No model provider configured yet."
		echo "    This bridge needs a real API key — there's no default that works"
		echo "    with zero setup. Pick one:"
		echo
		echo "    1) I have a Soket-shaped key (SOKET_API_KEY / TENSORSTUDIO_API_KEY / LOOP_API_KEY)"
		echo "    2) I want to set up a different provider (e.g. TensorStudio LiteLLM, Ollama)"
		echo "    3) I've already set this up manually — skip (I edited .env / models.json myself)"
		echo
		read -r -p "    Choice [1/2/3]: " provider_choice
		# .env not ending in a newline before an append merges the new line onto
		# the end of the last existing one instead of starting a fresh one — hit
		# repeatedly across several machines tonight (LOOP_SERVER_PROVIDER=...
		# silently glued onto the end of a key value, breaking both). Guard every
		# append below with this first.
		ensure_trailing_newline() {
			[ -s .env ] && [ -n "$(tail -c1 .env)" ] && echo >>.env
			return 0
		}
		if [ "$provider_choice" = "1" ]; then
			echo "    Paste ONLY the key itself (nothing shows while you type or paste)."
			ask_secret api_key "    API key: "
			ensure_trailing_newline
			echo "SOKET_API_KEY=${api_key}" >>.env
			echo "==> Saved to .env."
		elif [ "$provider_choice" = "2" ]; then
			# A custom provider needs *two* things registered, not just a key in
			# .env: (1) LOOP_SERVER_PROVIDER/MODEL + the key, here in .env, and
			# (2) the provider's id/URL/key-env-var/models registered in
			# ~/.loop/agent/models.json — a separate file, outside this repo
			# entirely, global to the machine. Missing just the second one is
			# exactly the failure this project kept hitting on fresh machines:
			# .env looks completely correct, the bridge boots fine, and it
			# still silently falls back to the built-in "soket" provider
			# because nothing registered the custom one anywhere the harness
			# actually checks. Doing both together here, in one guided step,
			# is the whole point of this branch.
			echo
			ask_field custom_id "    Provider id (e.g. tensorstudio-litellm): " '^[A-Za-z0-9._-]+$' "letters, digits, . _ - only (a short name, no spaces or '=')"
			ask_field custom_url "    Base URL (e.g. https://api.tensorstudio.ai/v1): " '^https?://[^[:space:]]+$' "a URL starting with http:// or https://"
			ask_field custom_model "    Model id (e.g. qwen3-8-27b): " '^[^[:space:]=]+$' "a single model id with no spaces or '='"
			ask_field custom_key_env "    Env var name for its key (e.g. TENSORSTUDIO_LITELLM_KEY): " '^[A-Z][A-Z0-9_]*$' "an UPPER_CASE variable name (the name only, not the key)"
			echo "    Paste ONLY the key itself (nothing shows while you type or paste)."
			ask_secret custom_key_value "    Key value: "
			if ! command -v python3 >/dev/null 2>&1; then
				echo "error: python3 not found — needed to safely edit models.json as JSON." >&2
				echo "       Add this manually to ~/.loop/agent/models.json's \"providers\" array instead:" >&2
				echo "       {\"id\": \"$custom_id\", \"name\": \"$custom_id\", \"baseUrl\": \"$custom_url\", \"apiKeyEnv\": [\"$custom_key_env\"], \"models\": [\"$custom_model\"]}" >&2
				exit 1
			fi
			mkdir -p "$HOME/.loop/agent"
			python3 - "$HOME/.loop/agent/models.json" "$custom_id" "$custom_url" "$custom_key_env" "$custom_model" <<'PYEOF'
import json, os, sys
path, pid, url, key_env, model = sys.argv[1:6]
data = {"providers": []}
if os.path.exists(path):
    try:
        with open(path) as f:
            data = json.load(f)
        data.setdefault("providers", [])
    except Exception:
        data = {"providers": []}
data["providers"] = [p for p in data["providers"] if p.get("id") != pid]
data["providers"].append({
    "id": pid,
    "name": pid,
    "baseUrl": url,
    "apiKeyEnv": [key_env],
    "models": [model],
})
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
print(f"    Registered {pid!r} in {path}")
PYEOF
			ensure_trailing_newline
			{
				echo "LOOP_SERVER_PROVIDER=${custom_id}"
				echo "LOOP_SERVER_MODEL=${custom_model}"
				echo "${custom_key_env}=${custom_key_value}"
			} >>.env
			echo "==> Saved provider config to .env and ~/.loop/agent/models.json."
		else
			echo "==> Skipping — make sure .env has LOOP_SERVER_PROVIDER/LOOP_SERVER_MODEL"
			echo "    and that provider's key set, AND that it's registered in"
			echo "    ~/.loop/agent/models.json, per bridge/README.md — otherwise this"
			echo "    will still boot fine and then fail silently on the first message."
		fi
		echo
	fi
fi

# Catches a .env that's already malformed (settings merged onto one line by a
# bad paste or an append without a line break) before anything is built —
# the bridge refuses to boot on the same condition, but failing here is
# faster and names the exact line.
merged_lines="$(grep -nE '^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*=.*[A-Z][A-Z0-9]*_[A-Z0-9_]+=' .env 2>/dev/null || true)"
if [ -n "$merged_lines" ]; then
	echo "error: .env has line(s) with several settings merged into one:" >&2
	echo "$merged_lines" | sed -E 's/^([0-9]+):([A-Za-z_][A-Za-z0-9_]*)=.*/       line \1: value of \2 contains another KEY=.../' >&2
	echo "       Each KEY=VALUE must be on its own line (see .env.example). Fix or delete" >&2
	echo "       those lines and re-run — nothing was built or started." >&2
	exit 1
fi

echo "==> Using config from .env"

if ! command -v cargo >/dev/null 2>&1; then
	echo
	echo "==> Rust isn't installed (no 'cargo' on PATH) — this bridge needs it to build."
	read -r -p "    Install it now via rustup.rs? [y/N]: " install_rust
	if [ "$install_rust" = "y" ] || [ "$install_rust" = "Y" ]; then
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
		# shellcheck disable=SC1091
		. "$HOME/.cargo/env"
		if ! command -v cargo >/dev/null 2>&1; then
			echo "error: rustup install finished but 'cargo' still isn't on PATH." >&2
			echo "       Open a new terminal (or run: source \$HOME/.cargo/env) and re-run this script." >&2
			exit 1
		fi
		echo "==> Rust installed."
		echo
	else
		echo "error: cargo not found — install Rust (https://rustup.rs) first, then re-run." >&2
		exit 1
	fi
fi
if ! command -v npm >/dev/null 2>&1; then
	echo "error: npm not found — install Node.js first." >&2
	exit 1
fi

if [ ! -d web/node_modules ]; then
	echo "==> Installing web/ dependencies (first run only)..."
	(cd web && npm install)
fi

# The frontend origin the bridge will allow — must match wherever Vite
# actually ends up running. Checked *before* starting anything: if this
# port is already taken, Vite silently picks a different one (e.g. 5174),
# the bridge still only allows 5173, and every message then fails in the
# browser with a generic, hard-to-diagnose "Failed to fetch" — CORS
# blocking a cross-origin request, not a real crash anywhere. Catching it
# here, with a clear message, beats debugging that after the fact.
WEB_PORT="${LOOP_SERVER_CORS_ORIGIN:-}"
WEB_PORT="${WEB_PORT##*:}"
# Falls back to 5173 both when unset (the normal case) and when it's not a
# plain number at all — e.g. once the Codespaces block below rewrites this
# to a full https://name-5173.app.github.dev URL, the naive ##*: strip
# above would otherwise yield "//name-5173.app.github.dev", not a port.
# Vite itself still always binds the plain local port regardless of what
# public URL fronts it, so 5173 is the right thing to check here either way.
case "$WEB_PORT" in
'' | *[!0-9]*) WEB_PORT=5173 ;;
esac
if lsof -i ":${WEB_PORT}" >/dev/null 2>&1; then
	echo "error: port ${WEB_PORT} is already in use by something else." >&2
	echo "       Vite would silently move to a different port, which breaks CORS" >&2
	echo "       against this bridge. Free port ${WEB_PORT} first (lsof -i :${WEB_PORT}" >&2
	echo "       to see what's using it), or set LOOP_SERVER_CORS_ORIGIN in .env to" >&2
	echo "       match whatever port you actually want to use." >&2
	exit 1
fi

# GitHub Codespaces runs the bridge and web server on a remote VM, not the
# machine the browser is on — localhost/127.0.0.1 (the defaults everywhere
# else) are simply wrong there; the browser needs each port's own public
# forwarding URL instead. Without this, getting both pieces talking to each
# other in a Codespace was a fully manual, error-prone dance: find both
# URLs in the Ports tab, hand-edit two separate .env files, restart,
# repeat every time the Codespace's name changes. Codespaces sets
# CODESPACE_NAME and GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN
# automatically — enough to construct both forwarding URLs ourselves,
# every run, with no manual step at all.
if [ -n "${CODESPACE_NAME:-}" ] && [ -n "${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN:-}" ]; then
	FORWARDED_BRIDGE_URL="https://${CODESPACE_NAME}-${LOOP_SERVER_PORT:-8787}.${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN}"
	FORWARDED_WEB_URL="https://${CODESPACE_NAME}-${WEB_PORT}.${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN}"

	echo "==> Detected GitHub Codespaces — using forwarded URLs instead of localhost:"
	echo "    bridge: ${FORWARDED_BRIDGE_URL}"
	echo "    web:    ${FORWARDED_WEB_URL}"

	# Replace-or-append the same way the provider setup above does — safe to
	# re-run on every launch without piling up duplicate/stale lines, and
	# picks up a new URL automatically if this Codespace's name changed
	# since last time (each new Codespace gets a new CODESPACE_NAME).
	[ -s .env ] && [ -n "$(tail -c1 .env)" ] && echo >>.env
	if grep -qE '^LOOP_SERVER_CORS_ORIGIN=' .env 2>/dev/null; then
		sed -i.bak "s|^LOOP_SERVER_CORS_ORIGIN=.*|LOOP_SERVER_CORS_ORIGIN=${FORWARDED_WEB_URL}|" .env && rm -f .env.bak
	else
		echo "LOOP_SERVER_CORS_ORIGIN=${FORWARDED_WEB_URL}" >>.env
	fi

	mkdir -p web
	[ -f web/.env ] || : >web/.env
	[ -s web/.env ] && [ -n "$(tail -c1 web/.env)" ] && echo >>web/.env
	if grep -qE '^VITE_LOOP_SERVER_URL=' web/.env 2>/dev/null; then
		sed -i.bak "s|^VITE_LOOP_SERVER_URL=.*|VITE_LOOP_SERVER_URL=${FORWARDED_BRIDGE_URL}|" web/.env && rm -f web/.env.bak
	else
		echo "VITE_LOOP_SERVER_URL=${FORWARDED_BRIDGE_URL}" >>web/.env
	fi
	echo "    (Ports 8787 and ${WEB_PORT} must both be set to Public visibility in the"
	echo "     Ports tab — Private ports need a separate GitHub auth step that a plain"
	echo "     browser fetch() can't complete on its own.)"
	echo
fi

echo "==> Building loop-server..."
cargo build -p loop-server

# Track both child PIDs so a single Ctrl+C tears down the whole stack —
# without this, killing the script leaves loop-server (and its held-open
# harness/session) running orphaned in the background.
PIDS=()
cleanup() {
	echo
	echo "==> Shutting down..."
	for pid in "${PIDS[@]}"; do
		# Negative PID = signal the whole process group, not just this one
		# process — see the `set -m` comment above for why that matters here.
		kill -- "-$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
	done
	wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "==> Starting loop-server on http://127.0.0.1:${LOOP_SERVER_PORT:-8787}..."
# RUST_LOG defaults to showing nothing at all — which looks identical to a
# hang or a crash from a blank terminal. Default to "info" here (unless the
# caller already set RUST_LOG) so the real boot sequence (booting
# AgentHarness / registered N tools / harness ready) is actually visible,
# not silent.
RUST_LOG="${RUST_LOG:-info}" ./target/debug/loop-server &
PIDS+=($!)

# Give loop-server a moment to bind before the frontend's first request —
# cosmetic only (the frontend just shows an error on first send and works
# fine on retry if this races), but avoids a confusing failed-request log
# line right at startup.
sleep 1

echo "==> Starting web frontend on http://localhost:5173..."
(cd web && npm run dev) &
PIDS+=($!)

wait
