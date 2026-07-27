# -*- mode: Python -*-
# Tiltfile — local development for MuggleBot.
#
# Not Kubernetes: MuggleBot is a local-first macOS app, so everything runs as
# local processes plus one container. `tilt up` starts the local Restate server
# (the durable-execution substrate), the Rust backend (rebuild on change) and the
# SolidJS/Vite UI (hot reload), and gives you on-demand test / clippy / fmt
# buttons in the Tilt UI.
#
#   tilt up      # start everything; Tilt UI at http://localhost:10350
#   tilt down    # stop everything
#
# The backend listen address is read from config.toml and passed to both the
# backend and the UI, so they always agree on the port.

DEFAULT_ADDR = "127.0.0.1:8080"

# --- Restate --------------------------------------------------------------
# Virtual-object state, invocation journals, durable timers and the vqueues live
# in the server; the record of signals/artifacts/secrets lives in SQLite. That
# split is what makes wiping ./data/restate a cheap operation (see below).
RESTATE_IMAGE = "docker.restate.dev/restatedev/restate:latest"
RESTATE_CONTAINER = "mugglebot-restate"
RESTATE_INGRESS_PORT = 8080  # where watchers submit signals
RESTATE_ADMIN_PORT = 9070  # admin API + web UI; also the health endpoint
RESTATE_NODE_PORT = 5122  # node-to-node
RESTATE_DATA = "./data/restate"
ENDPOINT_PORT = 9080  # where the backend serves its handlers

cfg = str(read_file("config.toml", default=""))
if cfg == "":
    warn(
        "config.toml not found — run `cp config.example.toml config.toml` first. "
        + "Assuming backend at %s." % DEFAULT_ADDR
    )

def resolve_listen(text, fallback):
    """Pull [ui].listen out of a TOML string without a real TOML parser."""
    section = ""
    addr = fallback
    for raw in text.splitlines():
        line = raw.strip()
        if line == "" or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line
        elif section == "[ui]" and line.startswith("listen"):
            parts = line.split("=", 1)
            if len(parts) == 2:
                value = parts[1].strip()
                if "#" in value:  # strip an inline comment
                    value = value.split("#", 1)[0].strip()
                addr = value.strip('"').strip("'")
    return addr

BACKEND_ADDR = resolve_listen(cfg, DEFAULT_ADDR)  # e.g. 127.0.0.1:8080
BACKEND_HOST = BACKEND_ADDR.split(":")[0]
BACKEND_PORT = int(BACKEND_ADDR.split(":")[-1])
UI_PORT = 5173
LINK_HOST = "localhost"  # friendlier for browser links than 127.0.0.1

if BACKEND_PORT == RESTATE_INGRESS_PORT:
    fail(
        "[ui].listen (%s) collides with the Restate ingress on port %d — "
        % (BACKEND_ADDR, RESTATE_INGRESS_PORT)
        + "move the UI to another port in config.toml (e.g. 127.0.0.1:8081)."
    )

# --- Restate server: local container ------------------------------------------
# vqueues (scope-based concurrency limits — one Ollama, one Chrome, bounded cloud
# spend) are experimental in 1.7 and can only be enabled on a cluster with no
# in-flight invocations. If the flags below don't take effect, the data dir
# predates them:
#
#   tilt down && rm -rf data/restate && tilt up
#
# That costs only in-flight invocations. Signals, artifacts, grounding and
# secrets are in SQLite and are untouched.
local_resource(
    "restate",
    cmd="docker rm -f %s >/dev/null 2>&1 || true" % RESTATE_CONTAINER,
    serve_cmd=" ".join(
        [
            "docker run --rm --name %s" % RESTATE_CONTAINER,
            "-p %d:8080" % RESTATE_INGRESS_PORT,
            "-p %d:9070" % RESTATE_ADMIN_PORT,
            "-p %d:5122" % RESTATE_NODE_PORT,
            "-v %s:/restate-data" % RESTATE_DATA,
            "--add-host=host.docker.internal:host-gateway",
            "-e RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true",
            "-e RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true",
            RESTATE_IMAGE,
            "--node-name mugglebot-1",
        ]
    ),
    labels=["restate"],
    links=[
        link("http://%s:%d" % (LINK_HOST, RESTATE_ADMIN_PORT), "Restate UI"),
        link(
            "http://%s:%d/health" % (LINK_HOST, RESTATE_ADMIN_PORT),
            "admin health",
        ),
    ],
    readiness_probe=probe(
        period_secs=5,
        http_get=http_get_action(
            port=RESTATE_ADMIN_PORT, host=BACKEND_HOST, path="/health"
        ),
    ),
)

# Restate discovers handlers at registration time, so adding a handler or changing
# a signature needs a re-register; --force makes that idempotent. The daemon does
# this itself on boot ([restate] register_on_boot), so this button is for when you
# want to read the discovery output — or to re-register after editing a handler
# without restarting the backend.
local_resource(
    "restate-register",
    cmd="restate --yes deployments register --force http://host.docker.internal:%d"
    % ENDPOINT_PORT,
    resource_deps=["restate", "backend"],
    labels=["restate"],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=False,
)

# What Restate is currently doing. The answer to "why has nothing been analyzed?"
# is usually an invocation that is queued or retrying, and that is invisible from
# MuggleBot's own logs.
local_resource(
    "restate-invocations",
    cmd="restate --yes sql \"SELECT target, status, scope, completion_result FROM sys_invocation ORDER BY created_at DESC LIMIT 30\"",
    labels=["restate"],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=False,
)

# --- Backend: cargo build -> run the binary -----------------------------------
# `cmd` is the build (compile errors surface as an update failure in the UI);
# `serve_cmd` runs the built binary and is restarted when a build succeeds.
local_resource(
    "backend",
    cmd="cargo build",
    serve_cmd="./target/debug/mugglebot --config config.toml",
    serve_env={"RUST_LOG": os.getenv("RUST_LOG", "info,mugglebot=debug")},
    deps=["src", "Cargo.toml", "Cargo.lock", "config.toml"],
    resource_deps=["restate"],  # handlers can't register before the server is up
    labels=["mugglebot"],
    links=[
        link("http://%s:%d/health" % (LINK_HOST, BACKEND_PORT), "health"),
        link("http://%s:%d/api/signals" % (LINK_HOST, BACKEND_PORT), "signals API"),
    ],
    readiness_probe=probe(
        period_secs=10,
        http_get=http_get_action(port=BACKEND_PORT, host=BACKEND_HOST, path="/health"),
    ),
)

# --- UI: Vite dev server (its own HMR handles src changes) --------------------
# We only re-run `npm ci` when the manifests change; unlike `npm install`, it
# does not rewrite the watched package-lock.json and trigger an update loop.
# Vite hot-reloads src itself, so Tilt just supervises the process. VITE_BACKEND
# points the UI at the backend address resolved above.
local_resource(
    "ui",
    cmd="cd ui && npm ci",
    serve_cmd="cd ui && npm run dev",
    serve_env={"VITE_BACKEND": BACKEND_ADDR},
    deps=["ui/package.json", "ui/package-lock.json"],
    labels=["mugglebot"],
    links=[link("http://%s:%d" % (LINK_HOST, UI_PORT), "LCARS UI")],
)

# --- On-demand checks (click to run in the Tilt UI) --------------------------
local_resource(
    "test",
    cmd="cargo test",
    deps=["src", "Cargo.toml"],
    labels=["checks"],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=False,
)

local_resource(
    "clippy",
    cmd="cargo clippy --all-targets",
    deps=["src", "Cargo.toml"],
    labels=["checks"],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=False,
)

local_resource(
    "fmt",
    cmd="cargo fmt --all",
    labels=["checks"],
    trigger_mode=TRIGGER_MODE_MANUAL,
    auto_init=False,
)
