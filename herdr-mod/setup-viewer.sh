#!/bin/sh
# Sets up a machine that views a Trellis Mac mini from its own Herdr app (#707).
#
# Mods run in the app that draws them, so the viewing machine needs its own copy of the
# `trellis` and `trellis-files` mods and of the `trellis` layout. This copies them from the
# mini over ssh, checks them, and switches the app to the layout. Run it ON THE VIEWING
# MACHINE, with the ssh host its Herdr uses for the mini:
#
#   ssh MINI 'cat ~/.config/herdr-app/mods/trellis/setup-viewer.sh' | sh -s MINI
#   sh setup-viewer.sh MINI                 # from a copy of the script
#
# It writes only ~/.config/herdr-app/mods/trellis, mods/trellis-files and layouts/trellis.json
# (an existing, different trellis.json is kept as trellis.json.bak). Herdr's shipped mods and
# layouts are never touched, and no token is copied: the mods read the mini's serve token on
# the mini, through `exec`. Running it again updates the copies.
#
# Environment: HERDR_APP_HOME (this machine's app home, default ~/.config/herdr-app);
# TRELLIS_REMOTE_APP_HOME (the mini's, relative to its home, default .config/herdr-app).

# Everything is in main(), called on the last line, so `sh -s` has read the whole script
# before anything runs; every command also reads /dev/null, never the piped script.
main() {
  set -eu
  me=setup-viewer
  die() { printf '%s: %s\n' "$me" "$*" >&2; exit 1; }
  say() { printf '%s: %s\n' "$me" "$*"; }

  host=${1:-}
  [ $# -eq 1 ] && [ -n "$host" ] || die "usage: sh setup-viewer.sh <ssh host of the mini>"
  case $host in -*) die "the ssh host can't start with '-': $host" ;; esac
  app=${HERDR_APP_HOME:-$HOME/.config/herdr-app}
  remote=${TRELLIS_REMOTE_APP_HOME:-.config/herdr-app}
  case $remote in *"'"*) die "TRELLIS_REMOTE_APP_HOME can't contain a single quote" ;; esac
  command -v ssh >/dev/null 2>&1 || die "ssh is not installed"
  command -v tar >/dev/null 2>&1 || die "tar is not installed"

  work=$(mktemp -d "${TMPDIR:-/tmp}/trellis-viewer.XXXXXX") || die "can't make a temporary folder"
  trap 'rm -rf "$work"' EXIT
  trap 'exit 130' INT TERM

  # 1. One archive of both mods and the layout. Drawings, editor typings and jsconfig.json
  #    are the app's own per-machine files, so they stay behind.
  say "copying the trellis mods and layout from $host"
  # shellcheck disable=SC2029 # $remote is expanded here on purpose, single-quoted for the mini.
  ssh "$host" "cd '$remote' 2>/dev/null || { echo 'no Herdr app home at ~/$remote' >&2; exit 3; }
    for p in mods/trellis/main.js mods/trellis/mod.json mods/trellis-files/main.js mods/trellis-files/mod.json layouts/trellis.json; do
      [ -f \"\$p\" ] || { echo \"missing ~/$remote/\$p (run 'node build.mjs --install' and clone trellis-files there; see herdr-mod/README.md)\" >&2; exit 4; }
    done
    tar --exclude .render --exclude '*.d.ts' --exclude jsconfig.json -czf - mods/trellis mods/trellis-files layouts/trellis.json" \
    </dev/null >"$work/bundle.tgz" || die "couldn't copy from $host (the message above says why)"
  mkdir "$work/x"
  tar -xzf "$work/bundle.tgz" -C "$work/x" </dev/null || die "the archive from $host is damaged"
  for p in mods/trellis/main.js mods/trellis-files/main.js layouts/trellis.json; do
    [ -f "$work/x/$p" ] || die "the archive from $host has no $p"
  done

  # 2. Install. Copying over the folders (rather than replacing them) keeps a running app's
  #    copies loaded; each saved file reloads them.
  mkdir -p "$app/mods" "$app/layouts" || die "can't write $app"
  for mod in trellis trellis-files; do
    { mkdir -p "$app/mods/$mod" && cp -R "$work/x/mods/$mod/." "$app/mods/$mod/"; } || die "couldn't install $app/mods/$mod"
  done
  layout=$app/layouts/trellis.json
  if [ -f "$layout" ] && ! cmp -s "$layout" "$work/x/layouts/trellis.json"; then
    cp "$layout" "$layout.bak" || die "couldn't keep $layout as $layout.bak"
    say "kept the previous layout as $layout.bak"
  fi
  cp "$work/x/layouts/trellis.json" "$layout" || die "couldn't install $layout"
  say "installed $app/mods/trellis, $app/mods/trellis-files and $layout"

  # 3 and 4. Check the mods, then show the layout.
  if ! command -v herdr-app >/dev/null 2>&1; then
    say "herdr-app isn't on PATH, so the mods weren't checked. In the Herdr app, switch to the trellis layout (ctrl-shift-l)."
    return 0
  fi
  for mod in trellis trellis-files; do
    herdr-app check "$app/mods/$mod" </dev/null || die "herdr-app check failed for $app/mods/$mod"
  done
  if herdr-app 'do' layout.switch '{"name":"trellis"}' </dev/null >/dev/null; then
    # Show the side panel once: the trellis pane then starts and turns on the tab bar's Files button.
    herdr-app 'do' layout.show '{"id":"panel"}' </dev/null >/dev/null || true
    herdr-app 'do' layout.select '{"stack":"panel-body","child":"files"}' </dev/null >/dev/null || true
    say "done: the Herdr app shows the trellis layout, with Files and the Trellis pane on the right"
  else
    say "installed and checked, but the Herdr app didn't switch layouts (is it open?). Open it, then run:"
    say "  herdr-app do layout.switch '{\"name\":\"trellis\"}'"
  fi
}

main "$@"
