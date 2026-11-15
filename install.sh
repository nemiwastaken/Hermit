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
# Everything lives under one prefix, PREFIX=/opt/hermit by default:
#   bin/hermit        binary
#   sandbox/          Dockerfile + server.py for the sandbox image
#   behavior/         your git repo: profile + skills
#   hermit.env        secrets and paths (mode 600)
#   data/hermit.db    SQLite database
#   run/              socket
#   workspace/        where the agent works
# Only the systemd unit sits outside: systemd only reads /etc.

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
CONF="$PREFIX/hermit.env"
DATA="$PREFIX/data"
RUN_DIR="$PREFIX/run"
WORKSPACE="$PREFIX/workspace"
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
install -d -m 700 -o root -g root "$DATA"
install -d -m 700 -o "$RUN_USER" -g "$RUN_GID" "$RUN_DIR"
install -d -m 755 -o "$RUN_USER" -g "$RUN_GID" "$WORKSPACE"

install -m 644 "$REPO/sandbox/server.py" "$REPO/sandbox/Dockerfile" "$PREFIX/sandbox/"
install -m 644 "$REPO/compose.yml" "$PREFIX/compose.yml"

cat > "$PREFIX/.env" <<EOF
# Written by install.sh: paths and the uid the sandbox runs as.
HERMIT_UID=$RUN_UID
HERMIT_GID=$RUN_GID
HERMIT_RUN_DIR=$RUN_DIR
HERMIT_WORKSPACE=$WORKSPACE
HERMIT_BEHAVIOR=$PREFIX/behavior
EOF
chmod 644 "$PREFIX/.env"

# Earlier installs kept the config in /etc and the database in /var/lib. Move
# them in once, so an upgrade does not lose the token or the queue. A leftover
# /srv/hermit is harmless: the container recreates socket and workspace.
if [ -e /etc/hermit/hermit.env ] || [ -e /var/lib/hermit/hermit.db ]; then
    say "moving the old split layout into $PREFIX"
    systemctl stop hermit 2>/dev/null || true
    if [ ! -f "$CONF" ] && [ -f /etc/hermit/hermit.env ]; then
        mv /etc/hermit/hermit.env "$CONF"
        rmdir /etc/hermit 2>/dev/null || true
    fi
    if [ ! -f "$DATA/hermit.db" ] && [ -f /var/lib/hermit/hermit.db ]; then
        mv /var/lib/hermit/hermit.db* "$DATA/"
        rmdir /var/lib/hermit 2>/dev/null || true
    fi
fi

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
PATHS=(
    -e "s|^DATABASE_PATH=.*|DATABASE_PATH=$DATA/hermit.db|"
    -e "s|^SANDBOX_SOCKET=.*|SANDBOX_SOCKET=$RUN_DIR/sandbox.sock|"
    -e "s|^BEHAVIOR_DIR=.*|BEHAVIOR_DIR=$PREFIX/behavior|"
)
if [ ! -f "$CONF" ]; then
    say "writing $CONF"
    sed "${PATHS[@]}" "$REPO/install/hermit.env.example" > "$CONF"
    chmod 600 "$CONF"

    # Prompts go through /dev/tty: with `curl | bash` stdin is the script itself.
    if { exec 3</dev/tty; } 2>/dev/null; then
        echo "   fill in now, or press enter and do it later in $CONF"
        ask() { IFS= read -r -p "   $1" "$2" <&3 || true; printf '\n'; }
        # Secrets stay off the screen.
        ask_hidden() { IFS= read -rs -p "   $1" "$2" <&3 || true; printf '\n'; }
        ask_hidden "Telegram bot token (@BotFather): " tg_token
        ask_hidden "LLM API key (DeepSeek): " llm_key
        ask "Allowed user ids, comma separated [optional, blank = ask the bot]: " users
        exec 3<&-
        esc() { printf '%s' "$1" | sed 's/[&/\]/\\&/g'; }
        if [ -n "${tg_token:-}" ]; then
            sed -i "s|^TELEGRAM_BOT_TOKEN=.*|TELEGRAM_BOT_TOKEN=$(esc "$tg_token")|" "$CONF"
        fi
        if [ -n "${llm_key:-}" ]; then
            sed -i "s|^LLM_API_KEY=.*|LLM_API_KEY=$(esc "$llm_key")|" "$CONF"
        fi
        if [ -n "${users:-}" ]; then
            sed -i "s|^TELEGRAM_ALLOWED_USER_IDS=.*|TELEGRAM_ALLOWED_USER_IDS=$(esc "$users")|" "$CONF"
        fi
        if [ -z "${tg_token:-}" ] || [ -z "${llm_key:-}" ]; then
            echo "   warning: without a token and an API key the service will not stay up"
        fi
    else
        echo "   non-interactive: fill in $CONF afterwards"
    fi
else
    say "keeping existing $CONF"
    # The paths belong to the install, not to the config: keep them pointing here.
    sed -i "${PATHS[@]}" "$CONF"
    # Configs written before the user allow-list existed get the key.
    grep -q '^TELEGRAM_ALLOWED_USER_IDS=' "$CONF" ||
        echo 'TELEGRAM_ALLOWED_USER_IDS=' >> "$CONF"
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
    sed "s|@PREFIX@|$PREFIX|g" "$REPO/core/systemd/hermit.service" > "$UNIT"
    chmod 644 "$UNIT"
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
  config     $CONF (mode 600)
  database   $DATA/hermit.db
  workspace  $WORKSPACE

Next:
  1. write to your bot in a private chat: it replies with your user id
  2. put that id into TELEGRAM_ALLOWED_USER_IDS in $CONF
  3. systemctl restart hermit
EOF
