# Source this before cargo on hosts without -devel packages (immutable Fedora):
#   source scripts/env.sh
# It creates native/lib (see setup-native-libs.sh) and points pkg-config at
# Homebrew's .pc files for headers. Binaries still link the system libraries.
_glance_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"$_glance_root/scripts/setup-native-libs.sh" >/dev/null
if command -v brew >/dev/null 2>&1; then
  _brew="$(brew --prefix)"
  export PKG_CONFIG_PATH="$_brew/lib/pkgconfig:$_brew/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi
unset _glance_root _brew
