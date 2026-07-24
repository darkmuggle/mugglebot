# -*- mode: Python -*-
# Tiltfile — local development for MuggleBot.
#
# Not Kubernetes: MuggleBot is a local-first macOS app, so everything runs as
# local processes. `tilt up` starts the Rust backend (rebuild on change) and the
# SolidJS/Vite UI (hot reload), and gives you on-demand test / clippy / fmt
# buttons in the Tilt UI.
#
#   tilt up      # start everything; Tilt UI at http://localhost:10350
#   tilt down    # stop everything
#
# The backend listen address is read from config.toml and passed to both the
# backend and the UI, so they always agree on the port.

DEFAULT_ADDR = "127.0.0.1:8080"

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

# --- Backend: cargo build -> run the binary -----------------------------------
# `cmd` is the build (compile errors surface as an update failure in the UI);
# `serve_cmd` runs the built binary and is restarted when a build succeeds.
local_resource(
    "backend",
    cmd="cargo build",
    serve_cmd="./target/debug/mugglebot --config config.toml",
    serve_env={"RUST_LOG": os.getenv("RUST_LOG", "info,mugglebot=debug")},
    deps=["src", "Cargo.toml", "Cargo.lock", "config.toml"],
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
