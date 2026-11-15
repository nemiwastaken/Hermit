#!/usr/bin/env bash
# Hermit installer.
#
#   sudo ./install.sh                 # from a clone: install or upgrade
#   curl -fsSL <url>/install.sh | sudo bash   # no clone needed
#   curl -fsSL <url>/install.sh | sudo bash -s -- --no-start
#
# Upgrade = run it again. The config, the database and behavior/ are never
# overwritten, and the binary is swapped atomically so a failure in an earlier
# step leaves the previous version running.
# Layout (the systemd unit assumes these default paths):
#   /opt/hermit/bin/hermit        binary
#   /opt/hermit/sandbox/          Dockerfile + server.py for the sandbox image
#   /opt/hermit/behavior/         your git repo: profile + skills
#   /etc/hermit/hermit.env        secrets and paths (mode 600)
#   /var/lib/hermit/              SQLite database
#   /srv/hermit/run/              socket
#   /srv/hermit/workspace/        where the agent works

set -euo pipefail

# Piped into bash there is no checkout around us: fetch the sources and re-exec
# from there. HERMIT_REF picks a branch/tag/commit, HERMIT_SRC a mirror tarball.
if [ -z "${BASH_SOURCE[0]:-}" ]; then
    REF="${HERMIT_REF:-main}"
    SRC="${HERMIT_SRC:-https://codeload.github.com/nemiwastaken/Hermit/tar.gz/$REF}"
    command -v curl >/dev/null || { echo "error: curl is required" >&2; exit 1; }
    command -v tar >/dev/null || { echo "error: tar is required" >&2; exit 1; }
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    echo "== downloading Hermit ($REF)"
    curl -fsSL "$SRC" | tar -xz -C "$TMP" --strip-components=1
    HERMIT_PIPED=1 HERMIT_GIT_REV="$REF" bash "$TMP/install.sh" "$@"
    exit $?
fi

PREFIX="${PREFIX:-/opt/hermit}"
CONF_DIR="${CONF_DIR:-/etc/hermit}"
DATA_DIR="${DATA_DIR:-/var/lib/hermit}"
SRV_DIR="${SRV_DIR:-/srv/hermit}"
UNIT=/etc/systemd/system/hermit.service

NO_SYSTEMD=0
NO_START=0
SKIP_BUILD=0

while [ $# -gt 0 ]; do
    case "$1" in
        --no-systemd) NO_SYSTEMD=1 ;;
        --no-start)   NO_START=1 ;;
        --skip-build) SKIP_BUILD=1 ;;
        -h|--help)    sed -n '2,/^$/{/^$/q;p}' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '\n== %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "$1 not found. $2"; }

[ "$(id -u)" -eq 0 ] || die "run as root: sudo bash $0"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[ -f "$REPO/compose.yml" ] || die "cannot find the repo at $REPO"

# Whose uid the sandbox container runs as, so files in the workspace belong to you.
RUN_USER="${SUDO_USER:-}"
if [ -z "$RUN_USER" ] || [ "$RUN_USER" = root ]; then
    RUN_USER="$(awk -F: '$3 >= 1000 && $3 < 65534 { print $1; exit }' /etc/passwd)"
fi
[ -n "$RUN_USER" ] || die "no normal user found, set SUDO_USER explicitly"
RUN_UID="$(id -u "$RUN_USER")"
RUN_GID="$(id -g "$RUN_USER")"
RUN_HOME="$(getent passwd "$RUN_USER" | cut -d: -f6)"

as_user() { runuser -u "$RUN_USER" -- env HOME="$RUN_HOME" "$@"; }

say "Hermit -> $PREFIX (sandbox runs as $RUN_USER:$RUN_GID)"
if [ -n "${HERMIT_GIT_REV:-}" ]; then
    GIT_REV="$HERMIT_GIT_REV"
else
    GIT_REV="$(git -C "$REPO" -c safe.directory="$REPO" describe --always --dirty 2>/dev/null || echo unknown)"
fi
echo "   source: $GIT_REV"
if [ -x "$PREFIX/bin/hermit" ]; then
    OLD_VERSION="$("$PREFIX/bin/hermit" --version 2>/dev/null || true)"
    echo "   updating existing install${OLD_VERSION:+ ($OLD_VERSION)}"
fi

# ---------------------------------------------------------------- build
if [ "$SKIP_BUILD" -eq 0 ]; then
    # Piped install extracts the sources as root into a 700 temp dir; the build
    # runs as the normal user, so hand it over.
    if [ -n "${HERMIT_PIPED:-}" ]; then
        chown -R "$RUN_USER:$RUN_GID" "$REPO"
    fi
    # Rust lives in the build user's home (rustup), rarely in root's PATH.
    CARGO="$(as_user bash -lc 'command -v cargo' 2>/dev/null || true)"
    [ -n "$CARGO" ] || CARGO="$RUN_HOME/.cargo/bin/cargo"
    [ -x "$CARGO" ] || die "cargo not found for $RUN_USER. Install Rust (https://rustup.rs) or pass --skip-build"
    as_user "$CARGO" build --release --manifest-path "$REPO/core/Cargo.toml"
fi
BIN="$REPO/core/target/release/hermit"
[ -x "$BIN" ] || BIN="$PREFIX/bin/hermit"
[ -x "$BIN" ] || die "no binary: build it first, or point --skip-build at an existing $PREFIX/bin/hermit"

# ---------------------------------------------------------------- layout
say "creating directories"
install -d -m 755 "$PREFIX" "$PREFIX/bin" "$PREFIX/sandbox"
install -d -m 700 -o root -g root "$DATA_DIR"
install -d -m 700 -o "$RUN_USER" -g "$RUN_GID" "$SRV_DIR/run"
install -d -m 755 -o "$RUN_USER" -g "$RUN_GID" "$SRV_DIR/workspace"

install -m 644 "$REPO/sandbox/server.py" "$REPO/sandbox/Dockerfile" "$PREFIX/sandbox/"
install -m 644 "$REPO/compose.yml" "$PREFIX/compose.yml"

cat > "$PREFIX/.env" <<EOF
# Written by install.sh: paths and the uid the sandbox runs as.
HERMIT_UID=$RUN_UID
HERMIT_GID=$RUN_GID
HERMIT_RUN_DIR=$SRV_DIR/run
HERMIT_WORKSPACE=$SRV_DIR/workspace
HERMIT_BEHAVIOR=$PREFIX/behavior
EOF
chmod 644 "$PREFIX/.env"

# ---------------------------------------------------------------- behavior
if [ ! -d "$PREFIX/behavior/.git" ]; then
    say "seeding $PREFIX/behavior"
    install -d -m 755 -o "$RUN_USER" -g "$RUN_GID" "$PREFIX/behavior"
    cp -a "$REPO/behavior.example/." "$PREFIX/behavior/"
    chown -R "$RUN_USER:$RUN_GID" "$PREFIX/behavior"
    as_user git -C "$PREFIX/behavior" init -q -b main
    as_user git -C "$PREFIX/behavior" add -A
    as_user git -C "$PREFIX/behavior" \
        -c user.name=hermit -c user.email=hermit@localhost \
        commit -qm "initial behavior"
    echo "   edit $PREFIX/behavior/user_profile.md and skills/*.md"
else
    say "keeping existing $PREFIX/behavior"
fi

# ---------------------------------------------------------------- config
if [ ! -f "$CONF_DIR/hermit.env" ]; then
    say "writing $CONF_DIR/hermit.env"
    install -d -m 700 "$CONF_DIR"
    sed -e "s|^DATABASE_PATH=.*|DATABASE_PATH=$DATA_DIR/hermit.db|" \
        -e "s|^SANDBOX_SOCKET=.*|SANDBOX_SOCKET=$SRV_DIR/run/sandbox.sock|" \
        -e "s|^BEHAVIOR_DIR=.*|BEHAVIOR_DIR=$PREFIX/behavior|" \
        "$REPO/install/hermit.env.example" > "$CONF_DIR/hermit.env"
    chmod 600 "$CONF_DIR/hermit.env"

    if [ -t 0 ]; then
        read -r -p "Telegram bot token: " tg_token || true
        read -r -p "LLM API key (DeepSeek): " llm_key || true
        read -r -p "Allowed chat ids, comma separated (blank = ask the bot for yours): " chats || true
        esc() { printf '%s' "$1" | sed 's/[&/\]/\\&/g'; }
        if [ -n "${tg_token:-}" ]; then
            sed -i "s|^TELEGRAM_BOT_TOKEN=.*|TELEGRAM_BOT_TOKEN=$(esc "$tg_token")|" "$CONF_DIR/hermit.env"
        fi
        if [ -n "${llm_key:-}" ]; then
            sed -i "s|^LLM_API_KEY=.*|LLM_API_KEY=$(esc "$llm_key")|" "$CONF_DIR/hermit.env"
        fi
        if [ -n "${chats:-}" ]; then
            sed -i "s|^TELEGRAM_ALLOWED_CHAT_IDS=.*|TELEGRAM_ALLOWED_CHAT_IDS=$(esc "$chats")|" "$CONF_DIR/hermit.env"
        fi
    else
        echo "   non-interactive: fill in $CONF_DIR/hermit.env afterwards"
    fi
else
    say "keeping existing $CONF_DIR/hermit.env"
fi

# ---------------------------------------------------------------- sandbox
if [ "$NO_START" -eq 0 ]; then
    say "starting the sandbox container"
    need docker "install Docker, or pass --no-start"
    ( cd "$PREFIX" && docker compose up -d --build )
fi

# ---------------------------------------------------------------- binary
# Atomic swap: writing over a running executable fails with "Text file busy",
# and a half-written binary must never be what systemd starts.
if [ "$BIN" = "$PREFIX/bin/hermit" ]; then
    say "binary already in place"
else
    say "installing the core binary"
    install -m 755 "$BIN" "$PREFIX/bin/.hermit.new"
    mv -f "$PREFIX/bin/.hermit.new" "$PREFIX/bin/hermit"
fi

# ---------------------------------------------------------------- service
if [ "$NO_SYSTEMD" -eq 0 ]; then
    say "installing hermit.service"
    install -m 644 "$REPO/core/systemd/hermit.service" "$UNIT"
    systemctl daemon-reload
    systemctl enable hermit >/dev/null
    if [ "$NO_START" -eq 0 ]; then
        if systemctl restart hermit; then
            echo "   $(systemctl is-active hermit) $(date -u +%FT%TZ)"
        else
            echo "   warning: service did not start, see: journalctl -u hermit -n 50"
        fi
    fi
else
    say "systemd skipped (--no-systemd)"
fi

VERSION="$("$PREFIX/bin/hermit" --version 2>/dev/null || echo unknown)"
cat <<EOF

Hermit installed:
  version    $VERSION ($GIT_REV)
  binary     $PREFIX/bin/hermit
  sandbox    $PREFIX/compose.yml -> container hermit-sandbox
  behavior   $PREFIX/behavior (git repo)
  config     $CONF_DIR/hermit.env
  database   $DATA_DIR/hermit.db
  workspace  $SRV_DIR/workspace

Next:
  1. write to your bot: it replies with your chat id
  2. put that id into TELEGRAM_ALLOWED_CHAT_IDS in $CONF_DIR/hermit.env
  3. systemctl restart hermit
EOF
