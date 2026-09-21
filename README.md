# loop-bridge

A browser chat UI for [Loop](https://github.com/soketlabs/loop)'s
`AgentHarness` — the same stateful agent the `loop` TUI uses, exposed
instead over HTTP/SSE/WebSocket to a web frontend.

## Requirements

`./scripts/dev.sh` checks all of these up front and tells you what's missing:

| Need | Version | Notes |
|---|---|---|
| **macOS or Linux** (or **WSL2** on Windows) | — | Native Windows shells (Git Bash/MSYS) are untested; use WSL2 |
| **Rust** (`cargo`) | 1.85+ | The script offers to install it via rustup; if it's older: `rustup update stable` |
| **C compiler / linker** | — | macOS: `xcode-select --install` · Debian/Ubuntu: `sudo apt install build-essential` |
| **git** | any | cargo fetches the harness from GitHub |
| **Node.js** | 20.19+ or 22.12+ | 22 LTS recommended (Vite 7's requirement) |
| **Free ports** | 5173 (web), 8787 (bridge) | Change via `LOOP_SERVER_CORS_ORIGIN` / `LOOP_SERVER_PORT` in `.env` |

The first build compiles the harness and takes a few minutes.

Open the app at `http://localhost:5173` or `http://127.0.0.1:5173` — both work.

## Quick start

**1. Clone and configure:**
```bash
git clone https://github.com/highlandzulu-cmd/loop-chat-bridge
cd loop-chat-bridge
cp .env.example .env
```

**2. Open `.env` in a text editor** and set a model provider — this is the
one thing you have to provide, there's no default that works with zero
setup. Two options, pick one:

- **Free, local, no API key**: install [Ollama](https://ollama.com), then
  follow `bridge/README.md`'s "Running against a free local model" section.
- **A hosted provider you already have a key for**: set
  `LOOP_SERVER_PROVIDER` / `LOOP_SERVER_MODEL` in `.env` and that
  provider's API key, per `bridge/README.md`.

**3. Run everything:**
```bash
./scripts/dev.sh
```
Opens on `http://localhost:5173`. First run also fetches the harness
automatically (a public dependency — no login, no key, nothing to set up
for that part) and installs `web/`'s npm packages.

**4. Confirm it's real**, not just "the page loaded": send a message. A
reply coming back means browser → bridge → harness → model are all
actually connected. If nothing comes back, see Troubleshooting below
before assuming something's badly broken — the most common causes are
quick, specific fixes.

## Troubleshooting

**Terminal running the bridge looks empty / "nothing is happening"**
Expected with no `RUST_LOG` set — the bridge doesn't log anything by
default, which looks identical to a hang. `./scripts/dev.sh` sets
`RUST_LOG=info` for you; running the binary directly, do the same:
```bash
RUST_LOG=info ./target/debug/loop-server
```
You should see `booting AgentHarness` → `registered 6 tools` →
`harness ready`. If instead you see nothing, it's genuinely still
running — confirm with `curl http://127.0.0.1:8787/health` from another
terminal rather than assuming it crashed.

**Browser shows "Error: Failed to fetch" right after sending a message**
Almost always a CORS mismatch, not a real crash — `curl` bypasses CORS
entirely, so if `curl http://127.0.0.1:8787/health` works fine but the
browser doesn't, this is it. Check the exact URL in your browser's
address bar: if it's not `http://localhost:5173` (Vite silently moves to
5174, 5175, etc. if 5173 is already taken by something else), either free
up 5173, or set `LOOP_SERVER_CORS_ORIGIN` in `.env` to match whatever port
you're actually on, then restart the bridge. `./scripts/dev.sh` checks
for this automatically before starting anything.

**A model-provider error like "Invalid model name" or HTTP 400 from the
provider itself** (not from this bridge)
The model name in `.env` doesn't match what that provider currently
serves — this happens especially on shared/testing endpoints where the
available model list changes over time. Check what's actually valid right
now, e.g. `curl https://api.tensorstudio.ai/v1/models -H "Authorization: Bearer $YOUR_KEY"`,
and update `LOOP_SERVER_MODEL` in `.env` to match.

**`npm install` fails with `EACCES: permission denied` inside
`~/.npm/_cacache`**
Not this project's bug — your machine's global npm cache has a
root-owned file in it, usually left over from a past `sudo npm install`.
Fix once, for every project on this machine:
```bash
sudo chown -R $(whoami):staff ~/.npm
```

## How this repo is put together

Two independent, sibling pieces in this repo, plus the real upstream
harness project — nothing here is copied or vendored from it:

| Piece | Where | Role |
|---|---|---|
| **the harness** | upstream: [`soketlabs/loop`](https://github.com/soketlabs/loop) | `AgentHarness` (Rust), unmodified — the actual agent loop, tools, LLM API |
| **the bridge** | [`bridge/`](bridge/README.md) (this repo) | HTTP/SSE/WebSocket server exposing that harness to a browser |
| **the frontend** | [`web/`](web/README.md) (this repo) | The chat UI a person actually uses, talking to the bridge |

Each piece is independently runnable and independently replaceable — the
bridge only depends on the harness through its public Cargo API (a git
dependency, pinned to a commit, not a copy), and the frontend only depends
on the bridge through its HTTP API (a configurable URL, not a build-time
link). See **[`chatui.md`](chatui.md) for the full system overview**
(architecture diagram, request lifecycle, RAG integration) — this file and
the two per-piece READMEs go deeper on each one.

- **bridge → harness**: `bridge/Cargo.toml` depends on `loop-agent`/
  `loop-ai`/`loop-app-core` as a Cargo git dependency against the public
  `soketlabs/loop` repo, pinned to a specific commit, resolved
  automatically on `cargo build` — no auth, no manual step. See
  [`bridge/README.md`](bridge/README.md#how-this-connects-to-the-harness).
- **web → bridge**: the frontend talks to the bridge over plain HTTP/SSE
  at a configurable URL (`VITE_LOOP_SERVER_URL`), with the bridge's
  `LOOP_SERVER_CORS_ORIGIN` pointed back at wherever the frontend is
  served from. See
  [`web/README.md`](web/README.md#connecting-this-to-a-bridge).

## To run just one piece

```bash
cargo run -p loop-server   # bridge only, port 8787
cd web && npm install && npm run dev   # frontend only, port 5173
```

## Just the harness (no bridge, no browser)

The harness has its own CLI and doesn't need any of this:

```bash
git clone https://github.com/soketlabs/loop
cd loop
cargo run -p loop-cli
```

See that repo's README for building on top of it as a library — that's
exactly what `bridge/` does.

## Build / test (this repo)

```bash
cargo build -p loop-server
cd web && npx tsc --noEmit
```

Harness-level tests (`loop-ai`, `loop-agent`, `loop-app-core`) live in and
run from the `soketlabs/loop` repo, not here.
