# Feature Factory operations

Trellis uses GitHub Issues as the durable intake queue for Feature Factory runs.
The tracked [`.factory.json`](../.factory.json) recognizes these exact references:

- `123`
- `#123`
- `https://github.com/SquashMerge/trellis/issues/123`

Other text remains a normal free-text feature request. A recognized reference is resolved with
`gh issue view` in `SquashMerge/trellis`; its issue number becomes the stable factory run ID and its
title and body become untrusted story input.

## Operator prerequisites

- Prime Agent with `prime-agent-feature-factory` installed.
- Node.js 22 or newer, Git, Rust, and GitHub CLI.
- GitHub CLI credentials for the `jasoncarreira` account.
- An inherited `GH_TOKEN` for `jasoncarreira` when the factory reaches an identity or publication gate.
- `FACTORY_PUBLISHING_IDENTITY=jasoncarreira` in the environment before starting a fresh run.

Before launching Prime Agent, fail closed on a missing token, bind Git transport to the same inherited
token in memory, and verify its GitHub identity without printing the credential:

```sh
if [ -z "${GH_TOKEN:-}" ]; then
  printf '%s\n' 'GH_TOKEN must be inherited and nonempty' >&2
  exit 1
fi
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=credential.helper \
  GIT_CONFIG_VALUE_0='!f() { echo username=x-access-token; echo "password=$GH_TOKEN"; }; f'
export FACTORY_PUBLISHING_IDENTITY=jasoncarreira
gh api --method GET /user --jq .login
```

The command must print `jasoncarreira`. Keep this environment on the Prime Agent process so guarded
`gh` operations and package-owned `git push` use the same identity. Do not run persistent
`gh auth setup-git`. Never commit `GH_TOKEN`, derive the declared publishing identity from the active
credential, or put credentials in `.factory.json`.

The publishing identity is deliberately not stored in `.factory.json`. Feature Factory records the
inherited declaration in the run and compares it with the authenticated GitHub login before external
publication.

## Starting a run

From a clean Trellis checkout on the desired PR base, invoke Feature Factory with one issue reference:

```text
/feature #123
```

The factory creates a private sandbox under `.factory-sandboxes/`, preserves its control plane under
`.factory/`, decomposes the approved issue into tested slices, and creates a draft PR after all gates
pass. Both directories are ignored by the tracked root `.gitignore`; they must never be committed.

Issue intake reads the issue body once when the run is created. A canonical-looking issue reference
that cannot be read fails the launch; it does not fall back to free text. Put requirement corrections in
the body, not comments. Resume is for transient external blockers. For changed requirements, preserve
anything worth keeping, deliberately remove the retained deterministic sandbox, and launch the issue
again so the story and plan are regenerated; invoking `/feature #123` again without removal resumes the
old manifest and story.

A good factory issue contains:

1. a concrete outcome and user value;
2. explicit scope and exclusions;
3. testable acceptance criteria;
4. dependencies on earlier issues;
5. security, provenance, and compatibility constraints;
6. validation evidence required before the issue is complete.

## Repository commands

The repository configuration has four operational boundaries:

- `resolve` reads a recognized GitHub Issue and emits the canonical factory payload.
- `bootstrap` installs the pinned MCP conformance-test packages from `tests/mcp` in a fresh sandbox without lifecycle scripts.
- `verify` runs the same Rust, MCP conformance, JavaScript syntax, and browser test commands as CI.
- `publish` declares the future branch-push command. The current factory release still performs its
  guarded Git and GitHub publication flow directly.

Configuration and `.gitignore` are privileged factory paths. Feature runs cannot modify them. Change
either only through a separately reviewed repository-maintenance PR.

## GitHub issue policy

The current semantic-index program is tracked in [#8](https://github.com/SquashMerge/trellis/issues/8),
with dependency-ordered stage epics [#9–#18](https://github.com/SquashMerge/trellis/milestone/1).

The staged semantic-index issues use the `epic`, `semantic-index`, and where applicable `mcp` labels.
Run one stage at a time after its dependencies are closed. Do not ask one run to implement later stages
implicitly. UI work is intentionally excluded from this program; the deliverable is a revision-pinned
semantic evidence index exposed to coding agents through read-only MCP tools.
