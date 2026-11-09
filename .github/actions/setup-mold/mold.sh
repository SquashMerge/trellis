# Sourced by action.yml: the pinned mold release and its published SHA-256s.
# shellcheck disable=SC2034 # the variables are used by action.yml
MOLD_VERSION=2.42.1
MOLD_ARCH=$(uname -m)
MOLD_DIR=$HOME/.cache/trellis-ci/mold
MOLD_TARBALL=$MOLD_DIR/mold-$MOLD_VERSION-$MOLD_ARCH-linux.tar.gz
MOLD_URL=https://github.com/rui314/mold/releases/download/v$MOLD_VERSION/mold-$MOLD_VERSION-$MOLD_ARCH-linux.tar.gz
case $MOLD_ARCH in
  x86_64) MOLD_SHA256=6ff270c9bf07d2bec5c98aa324eb7c4daf6a1a4d815c05ff1708049616047855 ;;
  aarch64) MOLD_SHA256=16b025652d3d7456689e6025a77e1903bb2a15e7630877c26cc133f5df95b9c6 ;;
  *) MOLD_SHA256= ;;
esac

mold_verify() {
  [ -n "$MOLD_SHA256" ] && printf '%s  %s\n' "$MOLD_SHA256" "$1" | sha256sum -c --quiet -
}
