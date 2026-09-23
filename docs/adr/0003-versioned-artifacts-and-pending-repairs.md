# Machine artifacts are versioned and repairs remain pending through verification

**Decision.** Every machine-consumed judgment, context, prewrite, and repair
artifact carries an explicit schema version and repository snapshot identity.
A repair that has replaced source files remains pending: its original files and
journal survive reindexing and replacement judgment. Success finalizes it;
failure restores the original source and rebuilds its index before returning.

**Why.** Snapshot identity prevents adapters from combining different source
generations, but it does not protect consumers when the artifact shape changes.
Schema versions make incompatible changes explicit. Likewise, atomic file
replacement alone is not a transaction when semantic verification happens
afterwards. Removing backups before replacement judgment turned a failed repair
into a committed mutation with an error message.

**Constraints.** Repair planning remains separate from application. Plans check
the source captured by their snapshot and parse all rewritten files before any
replacement. The pending phase may retain two repository snapshots and one
backup per touched file. Verification and cleanup are cold-path operations;
they must not enter write-time assessment. Journal-finalization failures retain
the journal while every backup still exists. After journal finalization, backup
deletion is garbage collection; failures report the orphan paths without
claiming that the committed repair remains recoverable through the journal.

**Consequences.** CLI, MCP, hooks, and later trace consumers can reject artifact
versions they do not understand. Repair callers must explicitly commit or roll
back a pending repair. A failed verification pays for a second reindex after
restoration; this is slower but preserves repository/index coherence.
