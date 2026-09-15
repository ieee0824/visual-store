# ADR 0004: explicit crash-resumable pruning

## Decision

Only `prune --apply` may remove internal representation objects. All ordinary store
handles retain a shared lock on the opened store directory; prune opens the directory
directly with an exclusive lock and never attempts a shared-to-exclusive upgrade.
`--dry-run` uses the same lock and verification snapshot.

A candidate is a distinct indexed PNG referenced only by non-deleted retired PNG
representations. Active PNGs, retained sources, segment color or alpha, and PNG
reconstruction references exclude it. Before mutation, every replacement segment for
every retired observation is fully verified through the normal segment-once verify
path. Missing codec support or any corrupt object, mapping, sample, or hash refuses
deletion.

Apply commits three durable phases:

1. update every candidate reference from `retained` to `pending` in SQLite;
2. unlink the exact hash-derived PNG filename and sync its parent directory;
3. update every `pending` reference to `deleted` in SQLite.

Retry includes `pending` candidates. It may repeat a harmless absent-file unlink and
then finalize the tombstone. `deleted` rows and blob metadata are retained as history;
verify expects their file to be absent unless another live reference later reuses the
same content hash.

## Capacity accounting

Reports sum distinct hashes within active representations, retained sources, and
retired candidates. Reclaimable and reclaimed bytes use actual regular-file lengths;
scoped physical bytes enumerate all files under the fixed-depth object tree before
and after. Thus shared objects are not multiplied by observation count, and one
invocation's reclaimed count equals the bytes it actually unlinked.
