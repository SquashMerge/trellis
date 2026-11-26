#!/bin/sh
# Feature Factory's repository verify, run on CI instead of on this machine.
#
# Usage (from .factory.json): sh tools/ci-verify.sh
#
# It pushes HEAD (the slice or merge commit the factory asks about) to
# qualify/factory-<sha>, where rust.yml runs the full verify matrix, waits for
# that commit's six matrix check runs and deletes the branch again on exit.
#
# Exit status, as the factory reads it (.factory.json):
#   0   all six required checks completed with success
#   1   a required check concluded failure or timed_out (repository verify failed)
#   75  verify could not run: the push URL is not GitHub, the push failed,
#       GitHub was unreachable for CI_VERIFY_OUTAGE_S, a required check had not
#       started after CI_VERIFY_START_WAIT_S, a required check concluded
#       cancelled, startup_failure, stale, action_required, skipped or neutral,
#       or CI_VERIFY_DEADLINE_S passed. 75 is `verify_unavailable_exit`: the
#       factory parks the slice instead of reviewing or retrying it. There is
#       no fallback to a local run.
#
# The deadline must stay below `verify_timeout_ms` in .factory.json, so the
# factory never kills verify (a killed verify has no exit status and burns a
# retry). Every git and gh call is bounded by CI_VERIFY_CALL_TIMEOUT_S for the
# same reason. The settings below are overridable for tests.
#
# GH_TOKEN, when set, comes from the factory's environment. Nothing here prints
# it or the push URL (which may carry credentials).
set -u

deadline_s=${CI_VERIFY_DEADLINE_S:-5100}         # 85 min; verify_timeout_ms is 90 min
start_wait_s=${CI_VERIFY_START_WAIT_S:-1200}     # 20 min for every required check to appear
outage_s=${CI_VERIFY_OUTAGE_S:-900}              # 15 min of failed API calls in a row
poll_s=${CI_VERIFY_POLL_S:-30}
call_timeout_s=${CI_VERIFY_CALL_TIMEOUT_S:-60}

# The six jobs of the `verify` matrix in .github/workflows/rust.yml, named
# "${{ matrix.job }} (${{ matrix.os }})" for job in [browser, tests, checks]
# and os in [ubuntu-latest, macos-latest]. Keep this list in step with that
# matrix; tools/ci-verify-test.py checks that it is.
required="browser (ubuntu-latest)
tests (ubuntu-latest)
checks (ubuntu-latest)
browser (macos-latest)
tests (macos-latest)
checks (macos-latest)"

say() { printf 'ci-verify: %s\n' "$*" >&2; }
unavailable() { say "unavailable: $*"; exit 75; }

started=$(date +%s)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/trellis-ci-verify.XXXXXX") || unavailable "cannot create a temporary directory"
ref=""
pushed=""
child=""
dog=""

# Runs "$@" with its output in $tmp/out and $tmp/err, killed after
# call_timeout_s so a hung connection cannot outlast the deadline.
bounded() {
  "$@" </dev/null >"$tmp/out" 2>"$tmp/err" &
  child=$!
  # The watchdog takes its sleep with it when it is stopped.
  (
    trap 'kill "$nap"; exit 0' TERM
    sleep "$call_timeout_s" &
    nap=$!
    wait "$nap" && kill "$child"
  ) </dev/null >/dev/null 2>&1 &
  dog=$!
  rc=0
  wait "$child" || rc=$?
  child=""
  kill "$dog" 2>/dev/null
  wait "$dog" 2>/dev/null
  dog=""
  return "$rc"
}

cleanup() {
  code=$?
  trap - EXIT INT TERM HUP
  [ -z "$child" ] || kill "$child" 2>/dev/null
  [ -z "$dog" ] || kill "$dog" 2>/dev/null
  # Delete only the branch this run pushed.
  if [ -n "$pushed" ]; then
    if bounded git push origin --delete "$ref"; then
      say "deleted $ref"
    else
      say "could not delete $ref; delete it by hand"
    fi
  fi
  rm -rf "$tmp"
  exit "$code"
}
trap cleanup EXIT
trap 'exit 75' INT TERM HUP

# The commit to check, before anything else can move HEAD.
bounded git rev-parse HEAD || unavailable "git rev-parse HEAD failed"
sha=$(cat "$tmp/out")
case $sha in
  *[!0-9a-f]* | "") unavailable "HEAD is not a commit id" ;;
esac
ref="refs/heads/qualify/factory-$sha"

# The factory sandbox's origin fetches from a local path, so gh must never infer
# the repository: it comes from the push URL and is named in every API path.
bounded git remote get-url --push origin || unavailable "origin has no push URL"
url=$(cat "$tmp/out")
case $url in
  https://github.com/*) repo=${url#https://github.com/} ;;
  git@github.com:*) repo=${url#git@github.com:} ;;
  ssh://git@github.com/*) repo=${url#ssh://git@github.com/} ;;
  *) unavailable "origin's push URL is not a GitHub repository URL" ;;
esac
repo=${repo%.git}
owner=${repo%%/*}
name=${repo#*/}
case $owner/$name in
  */*/* | /* | */ | *[!A-Za-z0-9._/-]*) unavailable "origin's push URL is not a GitHub repository URL" ;;
esac
[ "$owner/$name" = "$repo" ] || unavailable "origin's push URL is not a GitHub repository URL"

# One page-set of the commit's check runs from GitHub Actions, as
# "id<TAB>name<TAB>status<TAB>conclusion" lines in $tmp/runs. Status 1 is an
# API error; the caller retries until the outage window closes.
last_ok=$started
fetch() {
  if bounded gh api --paginate "repos/$repo/commits/$sha/check-runs?filter=all&per_page=100" \
    --jq '.check_runs[] | select(.app.slug == "github-actions") | [.id, .name, .status, (.conclusion // "")] | @tsv'; then
    mv "$tmp/out" "$tmp/runs"
    last_ok=$(date +%s)
    return 0
  fi
  say "GitHub API call failed: $(head -c 300 "$tmp/err" | tr '\n' ' ')"
  [ $(($(date +%s) - last_ok)) -lt "$outage_s" ] || unavailable "GitHub unreachable for ${outage_s} s"
  return 1
}

past_deadline() {
  [ $(($(date +%s) - started)) -ge "$deadline_s" ]
}

# Check runs that already exist for this commit belong to earlier pushes; only
# runs created after this push (higher ids) answer it.
until fetch; do
  past_deadline && unavailable "deadline of ${deadline_s} s reached"
  sleep "$poll_s"
done
baseline=$(awk -F '\t' 'BEGIN { m = 0 } $1 + 0 > m { m = $1 + 0 } END { print m }' "$tmp/runs")

bounded git push --porcelain origin "$sha:$ref" || unavailable "git push of $ref failed: $(head -c 300 "$tmp/err" | tr '\n' ' ')"
pushed=1
pushed_at=$(date +%s)
# A branch left behind by an earlier, killed verify already points here; the
# push started nothing new, so that branch's runs are the answer.
if grep -q '^=' "$tmp/out"; then
  baseline=0
fi
say "pushed $sha to qualify/factory-$sha in $repo; waiting for its six rust.yml checks"

while :; do
  if fetch; then
    # Newest run (highest id) per name among this push's runs, then one line
    # per required check: "<state> <conclusion-or-status> <name>".
    printf '%s\n' "$required" >"$tmp/required"
    awk -F '\t' -v base="$baseline" '
      NR == FNR { want[$0] = 1; order[++n] = $0; next }
      ($2 in want) && $1 + 0 > base && $1 + 0 > id[$2] + 0 { id[$2] = $1; st[$2] = $3; co[$2] = $4 }
      END {
        for (i = 1; i <= n; i++) {
          k = order[i]
          if (!(k in id)) { print "missing - " k; continue }
          if (st[k] != "completed") { print "pending " st[k] " " k; continue }
          if (co[k] == "success") print "success success " k
          else if (co[k] == "failure" || co[k] == "timed_out") print "failed " co[k] " " k
          else print "unavailable " co[k] " " k
        }
      }' "$tmp/required" "$tmp/runs" >"$tmp/verdict"
    if grep -q '^failed ' "$tmp/verdict"; then
      sed -n 's/^failed \([^ ]*\) \(.*\)/ci-verify: \2 concluded \1/p' "$tmp/verdict" >&2
      say "repository verify failed for $sha"
      exit 1
    fi
    if grep -q '^unavailable ' "$tmp/verdict"; then
      unavailable "$(sed -n 's/^unavailable \([^ ]*\) \(.*\)/\2 concluded \1;/p' "$tmp/verdict" | tr '\n' ' ')"
    fi
    if ! grep -qv '^success ' "$tmp/verdict"; then
      say "all six required checks passed for $sha"
      exit 0
    fi
    if grep -q '^missing ' "$tmp/verdict" && [ $(($(date +%s) - pushed_at)) -ge "$start_wait_s" ]; then
      unavailable "not started after ${start_wait_s} s: $(sed -n 's/^missing - //p' "$tmp/verdict" | tr '\n' ';')"
    fi
  fi
  past_deadline && unavailable "deadline of ${deadline_s} s reached; still waiting: $(grep -v '^success ' "$tmp/verdict" 2>/dev/null | sed 's/^[^ ]* [^ ]* //' | tr '\n' ';')"
  sleep "$poll_s"
done
