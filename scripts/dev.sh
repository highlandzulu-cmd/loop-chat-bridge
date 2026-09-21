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

BIN_EXT=""
case "$(uname -s 2>/dev/null)" in
MINGW* | MSYS* | CYGWIN*)
	echo "warning: native Windows shells (Git Bash/MSYS/Cygwin) are untested here." >&2
	echo "         If anything below fails, use WSL2 (Ubuntu) instead." >&2
	BIN_EXT=".exe"
	;;
Darwin)
	if ! xcode-select -p >/dev/null 2>&1; then
		echo "error: Xcode command line tools are missing (Rust needs a linker)." >&2
		echo "       Run: xcode-select --install   then re-run this script." >&2
		exit 1
	fi
	;;
Linux)
	if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
		echo "error: no C compiler/linker found (Rust needs one)." >&2
		echo "       Debian/Ubuntu: sudo apt install build-essential   Fedora: sudo dnf groupinstall 'Development Tools'" >&2
		exit 1
	fi
	;;
esac
if ! command -v git >/dev/null 2>&1; then
	echo "error: git not found — cargo needs it to fetch the loop harness dependency." >&2
	exit 1
fi

# Read a setting the same way the bridge does: a real env var wins, else the
# first matching KEY=VALUE line in .env. dev.sh used to look only at the
# shell environment, so values set in .env (the documented place) were
# ignored by the port checks and URLs below.
dotenv_get() {
	local key="$1" val
	val="${!key:-}"
	if [ -z "$val" ] && [ -f .env ]; then
		val="$(grep -E "^[[:space:]]*${key}=" .env 2>/dev/null | head -n1 | tr -d '\r' | sed -E "s/^[^=]*=//; s/^[\"']//; s/[\"']\$//")" || true
	fi
	printf '%s' "$val"
}

# Is something already listening on this TCP port? lsof isn't installed on
# many minimal Linux setups, so fall back to a bash /dev/tcp probe.
port_in_use() {
	if command -v lsof >/dev/null 2>&1; then
		lsof -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1
	else
		(exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
	fi
}

# Prompts below need a real terminal; without one, `read` hits EOF and
# `set -e` exits with no explanation.
require_tty() {
	if [ ! -t 0 ]; then
		echo "error: $1 — and there's no terminal to ask you. Run this from an interactive shell." >&2
		exit 1
	fi
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
		require_tty "no model provider is configured"
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
			read -r -s -p "    Paste your API key: " api_key
			echo
			if [ -n "$api_key" ]; then
				ensure_trailing_newline
				echo "SOKET_API_KEY=${api_key}" >>.env
				echo "==> Saved to .env."
			else
				echo "error: no key entered — nothing saved. Add one to .env manually and re-run." >&2
				exit 1
			fi
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
			read -r -p "    Provider id (e.g. tensorstudio-litellm): " custom_id
			read -r -p "    Base URL (e.g. https://api.tensorstudio.ai/v1): " custom_url
			read -r -p "    Model id (e.g. qwen3-8-27b): " custom_model
			read -r -p "    Env var name for its key (e.g. TENSORSTUDIO_LITELLM_KEY): " custom_key_env
			read -r -s -p "    Paste the actual key value: " custom_key_value
			echo
			if [ -z "$custom_id" ] || [ -z "$custom_url" ] || [ -z "$custom_model" ] || [ -z "$custom_key_env" ] || [ -z "$custom_key_value" ]; then
				echo "error: all five fields are required — nothing saved. Re-run and fill in each one." >&2
				exit 1
			fi
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

echo "==> Using config from .env"

if ! command -v cargo >/dev/null 2>&1; then
	echo
	echo "==> Rust isn't installed (no 'cargo' on PATH) — this bridge needs it to build."
	require_tty "cargo isn't installed"
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

need_rust_minor="$(sed -nE 's/^rust-version *= *"1\.([0-9]+).*/\1/p' Cargo.toml | head -n1)" || true
have_rust_minor="$(rustc --version 2>/dev/null | sed -nE 's/^rustc 1\.([0-9]+).*/\1/p')" || true
if [ -n "$need_rust_minor" ] && [ -n "$have_rust_minor" ] && [ "$have_rust_minor" -lt "$need_rust_minor" ]; then
	echo "error: Rust 1.${need_rust_minor}+ required, found $(rustc --version)." >&2
	echo "       Update with: rustup update stable" >&2
	exit 1
fi

if ! command -v npm >/dev/null 2>&1 || ! command -v node >/dev/null 2>&1; then
	echo "error: node/npm not found — install Node.js 22 LTS (https://nodejs.org) first." >&2
	exit 1
fi
# Vite 7 (web/package.json) needs Node ^20.19 or >=22.12; older versions
# fail with a cryptic syntax/engine error after the slow Rust build.
node_ok="$(node -e 'const [a,b]=process.versions.node.split(".").map(Number);console.log(a>22||(a===22&&b>=12)||(a===20&&b>=19)?"ok":"old")')"
if [ "$node_ok" != "ok" ]; then
	echo "error: Node $(node -v) is too old — need 20.19+ or 22.12+ (22 LTS recommended)." >&2
	exit 1
fi

if [ ! -d web/node_modules ]; then
	echo "==> Installing web/ dependencies (first run only)..."
	# `npm ci` installs exactly what package-lock.json pins.
	(cd web && { npm ci || npm install; })
fi

# The frontend origin the bridge will allow — must match wherever Vite
# actually ends up running. Checked *before* starting anything: if this
# port is already taken, Vite silently picks a different one (e.g. 5174),
# the bridge still only allows 5173, and every message then fails in the
# browser with a generic, hard-to-diagnose "Failed to fetch" — CORS
# blocking a cross-origin request, not a real crash anywhere. Catching it
# here, with a clear message, beats debugging that after the fact.
BRIDGE_PORT="$(dotenv_get LOOP_SERVER_PORT)"
BRIDGE_PORT="${BRIDGE_PORT:-8787}"
WEB_PORT="$(dotenv_get LOOP_SERVER_CORS_ORIGIN)"
WEB_PORT="${WEB_PORT%%,*}"
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
if port_in_use "$WEB_PORT"; then
	echo "error: port ${WEB_PORT} is already in use by something else." >&2
	echo "       Free it first (lsof -iTCP:${WEB_PORT} -sTCP:LISTEN shows what's using it)," >&2
	echo "       or set LOOP_SERVER_CORS_ORIGIN in .env to match a different port." >&2
	exit 1
fi
if port_in_use "$BRIDGE_PORT"; then
	echo "error: port ${BRIDGE_PORT} (the bridge) is already in use — possibly a previous" >&2
	echo "       run of this script that didn't shut down. Free it, or set LOOP_SERVER_PORT in .env." >&2
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
	FORWARDED_BRIDGE_URL="https://${CODESPACE_NAME}-${BRIDGE_PORT}.${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN}"
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

# Pin the target dir so the binary path below is always right — a global
# `build.target-dir` in ~/.cargo/config.toml or a CARGO_TARGET_DIR export
# would otherwise put it somewhere other than ./target.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
echo "==> Building loop-server (first build takes a few minutes)..."
cargo build -p loop-server

# Track both child PIDs so a single Ctrl+C tears down the whole stack —
# without this, killing the script leaves loop-server (and its held-open
# harness/session) running orphaned in the background.
PIDS=()
cleanup() {
	echo
	echo "==> Shutting down..."
	# ${arr[@]+...} form: bash 3.2 (macOS default) errors on an empty array under set -u.
	for pid in ${PIDS[@]+"${PIDS[@]}"}; do
		# Negative PID = signal the whole process group, not just this one
		# process — see the `set -m` comment above for why that matters here.
		kill -- "-$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
	done
	wait 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

echo "==> Starting loop-server on http://127.0.0.1:${BRIDGE_PORT}..."
# RUST_LOG defaults to showing nothing at all — which looks identical to a
# hang or a crash from a blank terminal. Default to "info" here (unless the
# caller already set RUST_LOG) so the real boot sequence is visible.
RUST_LOG="${RUST_LOG:-info}" "$CARGO_TARGET_DIR/debug/loop-server${BIN_EXT}" &
BRIDGE_PID=$!
PIDS+=($BRIDGE_PID)

# Wait until the bridge actually answers /health (up to 60s) instead of a
# fixed sleep — on a slow machine it can take longer than a second to boot,
# and if it exits (bad provider config, etc.) fail right here with its log
# above rather than serving a UI that can never connect.
if command -v curl >/dev/null 2>&1; then
	ready=0
	for _ in $(seq 1 120); do
		if curl -fsS "http://127.0.0.1:${BRIDGE_PORT}/health" >/dev/null 2>&1; then
			ready=1
			break
		fi
		if ! kill -0 "$BRIDGE_PID" 2>/dev/null; then
			echo "error: loop-server exited during startup — see its output above." >&2
			exit 1
		fi
		sleep 0.5
	done
	if [ "$ready" != "1" ]; then
		echo "error: loop-server didn't answer /health within 60s — see its output above." >&2
		exit 1
	fi
else
	sleep 3
fi

# If the bridge port isn't the default and the frontend wasn't told where it
# is, tell it — otherwise the UI keeps calling 8787 and every request fails.
if [ -z "${VITE_LOOP_SERVER_URL:-}" ] && ! grep -qE '^VITE_LOOP_SERVER_URL=' web/.env 2>/dev/null; then
	export VITE_LOOP_SERVER_URL="http://127.0.0.1:${BRIDGE_PORT}"
fi

echo "==> Starting web frontend on http://localhost:${WEB_PORT}..."
(cd web && exec npm run dev -- --port "$WEB_PORT" --strictPort) &
PIDS+=($!)

# Stop everything if either process dies, rather than leaving a half-dead stack.
while :; do
	for pid in "${PIDS[@]}"; do
		if ! kill -0 "$pid" 2>/dev/null; then
			echo "error: a child process (pid $pid) exited — shutting down." >&2
			exit 1
		fi
	done
	sleep 1
done
