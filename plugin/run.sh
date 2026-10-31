#!/bin/sh
# Locate herdr-infinity for a plugin invocation.
#
# Plugin commands inherit the Herdr *server's* environment, not the user's
# shell. A server started from a desktop session or a login shell typically has
# only the system PATH, so neither `~/.cargo/bin` (cargo install) nor
# `~/.local/bin` is visible and a bare `herdr-infinity` command fails with
# ENOENT. Resolve the binary explicitly instead.
#
# Order: an explicit override, then the plugin's own build output (for a linked
# checkout), then the usual install prefixes, then PATH as a last resort.
set -eu

# Derive the plugin root from this script's own location when Herdr has not
# supplied it. Done with parameter expansion rather than `dirname`, because
# this runs with the Herdr server's environment: PATH may not contain
# coreutils, and the fallback must not depend on an external command. Avoiding
# `cd` also means a stray CDPATH cannot interfere.
if [ -n "${HERDR_PLUGIN_ROOT:-}" ]; then
    root=$HERDR_PLUGIN_ROOT
else
    # "$0" -> its directory -> that directory's parent.
    self_dir=${0%/*}
    [ "$self_dir" = "$0" ] && self_dir=.
    root=${self_dir%/*}
    [ "$root" = "$self_dir" ] && root=..
fi

candidates="
${HERDR_INFINITY_BIN:-}
$root/target/release/herdr-infinity
$root/target/debug/herdr-infinity
${CARGO_HOME:-$HOME/.cargo}/bin/herdr-infinity
$HOME/.local/bin/herdr-infinity
/usr/local/bin/herdr-infinity
/usr/bin/herdr-infinity
"

# Word splitting is the point here: $candidates is a newline-separated list.
# shellcheck disable=SC2086
for candidate in $candidates; do
    [ -n "$candidate" ] || continue
    if [ -x "$candidate" ]; then
        exec "$candidate" "$@"
    fi
done

if command -v herdr-infinity >/dev/null 2>&1; then
    exec herdr-infinity "$@"
fi

echo "herdr-infinity: binary not found." >&2
echo "Install it with \`cargo install --path $root\`, or set" >&2
echo "HERDR_INFINITY_BIN to its absolute path." >&2
exit 127
