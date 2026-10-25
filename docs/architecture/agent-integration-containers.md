# Proposed agent integration — containers

Not implemented. The per-checkout index and stdio MCP parts follow the owner-accepted
[local topology](../local-topology.md), whose mechanics Stage 1 ratifies; the registry, Mimir, Herdr,
terminal and artifact parts remain proposals with their own later slices. The stdio MCP server, per-checkout indexes, watcher leadership, file watcher, registry,
Mimir Trellis provider and artifact service shown here must be implemented. Existing Hands v1 does not
accept arbitrary tools. This is the target architecture: artifact storage, embedded terminal
streaming and ACP client sessions are separate slices. See the [local topology](../local-topology.md).

```mermaid
C4Container
  title Trellis agent integration - proposed containers
  Person(user, "Developer", "Chooses what to inspect")
  Container_Ext(agent, "Agent client", "Existing harness", "Calls Trellis tools directly")
  Container_Ext(mimirRuntime, "Mimir runtime", "Existing daemon and ACP", "Runs Mimir agent turns")
  Container_Ext(proxy, "Mimir local proxy", "Existing ACP proxy plus new provider", "Mediates local tool permissions")
  Container_Ext(herdr, "Herdr server", "Optional local terminal service", "Owns panes and lifecycle metadata")
  System_Boundary(trellis, "Trellis") {
    Container(ui, "Workbench", "Local browser UI", "Source, diagrams, terminal and ACP tabs")
    Container(mcp, "trellis mcp", "stdio MCP, one per agent session", "Serves the checkout it was launched in; read-only tools")
    Container(core, "Browser daemon", "Rust", "Serves the workbench; stores artifacts")
    ContainerDb(registry, "Project registry", "Optional local durable store", "Lists checkouts for the browser picker; not needed by agents")
    ContainerDb(stores, "Per-checkout index", "SQLite cache, per-user, keyed by path", "Disposable evidence; written only by the leader")
    ContainerDb(durable, "Durable state", "SQLite outside the checkout", "Proposed artifacts and tokens; retired records ignored")
    ContainerDb(facts, "Shared fact cache", "SQLite, per-user cache", "Per-file extraction by content hash")
  }
  Rel(user, ui, "Inspects code and operates agent tabs", "Local HTTP")
  Rel(core, agent, "Optionally launches an explicitly owned child", "Real PTY; existing harness")
  Rel(core, ui, "Streams owned or validated attached terminals", "Authenticated bounded channel")
  Rel(core, proxy, "Optionally acts as the chosen ACP client", "Negotiated ACP")
  Rel(agent, mcp, "Launches and calls tools", "MCP stdio")
  Rel(mimirRuntime, proxy, "Requests local tools through existing channel", "ACP tool bridge")
  Rel(proxy, mcp, "Invokes versioned Trellis capability", "Proposed provider adapter")
  Rel(mcp, stores, "Reads snapshots; watches and writes when leader", "SQLite")
  Rel(mcp, facts, "Reuses unchanged-file extraction", "Content hash")
  Rel(ui, core, "Queries projects and artifacts", "Authenticated local API")
  Rel(core, ui, "Notifies of available artifact versions", "Proposed metadata event stream")
  Rel(core, registry, "Looks up authorized project bindings", "Local storage")
  Rel(core, stores, "Reads snapshots; watches and writes when leader", "SQLite")
  Rel(core, durable, "Persists proposed artifacts", "SQLite")
  Rel(core, herdr, "Optionally discovers and reconciles metadata", "Versioned local socket adapter")
```

There is no new ACP observer connection beside an existing editor client. The local proxy and
Trellis run on the machine holding the inspected workspace; a remote daemon's paths are different.
The Herdr adapter is a daemon component, not another agent harness or replacement pane manager.
Herdr metadata/snapshot APIs do not establish support for raw per-pane terminal embedding. Validate
a supported attach transport before adding that route. ACP protocol stdout is not terminal output.
See the [terminal contract](../terminal-workbench-contract.md).

[Plan and boundaries](../agent-integration-plan.md).
