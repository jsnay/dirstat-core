# Deferrals

Spec'd but intentionally not built yet, with reasons (kickoff-prompt rule:
deferrals are explicit). Nothing here blocks the design-review scope
(1b / 1d / 1e / 1f / 1g), which is fully served by the current ABI.

| Spec ID | What | Why deferred |
|---|---|---|
| CORE-TM-3 | Cushion surface coefficients | The shipped design defaults to flat fills + hairline nesting + a subtle per-rect gradient; "Classic cushions" is a view option for a later phase. |
| CORE-TM-2 (partial) | KDirStat row layout is a simple strip layout | Both algorithms are area-correct and selectable; the classic alternating-rows refinement can land when the "classic" view option does. |
| CORE-IO-1/2/3 | Save / reload scans | Phase 5 in the kickoff plan; no UI consumes it yet ("no orphan code"). |
| CORE-SCAN-8 / CORE-OPT-4 | Opaque-bundle option | The kind classifier already treats bundles as one *category*; opaque sizing is a settings-phase feature. |
| CORE-SCAN-14 | Multiple roots per model | The design's first-run picker (1f) scans one volume/folder per window. |
| CORE-OPT-3 | Firmlink/alias following | macOS-specific edge; needs real-device validation. |
| CORE-EXT-1 (partial) | Physical bytes per extension | Logical bytes drive the legend/table in the design; physical per-type adds a second aggregation pass nothing displays yet. |
| CORE-SCAN-12 (partial) | `refresh_node` does not re-aggregate extension stats for the refreshed subtree | Post-delete refresh keeps tree totals exact; type-table drift after deletes is bounded and vanishes on rescan. Fix lands with save/reload phase. |
| CORE-PERF-1/2, CORE-MEM-1 | Recorded perf baselines (`perf-baseline.json`) | Needs a stable reference machine; budgets are machine-relative by spec. |
| CORE-SCAN-1 (partial) | ctime + `getattrlistbulk` fast path | mtime ships (age channel needs it); ctime and the macOS bulk-stat syscall are perf work, not correctness. |
