# ADR 0005: External judgment layer

## Status

Accepted for storage format version 3.

## Context

Visual Store records local screenshot observations behind opaque `visual://` references. Saving an observation is intentionally separate from materializing or displaying its image. UI agents also need to attach typed decisions—rules, classical algorithms, Jev, vision models, and human review—without turning `put` into a networked inference operation.

[Jev](https://docs.typesafe.ai/introduction) accepts state plus typed questions and returns structured decisions. The existing [jev-mcp](https://github.com/ieee0824/jev-mcp) server is the integration boundary: the caller supplies the state, and the server does not inspect this repository or Visual Store by itself. Visual Store therefore must persist inputs already available locally and results returned by an external caller, but must not import a Jev SDK, manage an API key, or call a remote service.

## Decision

Format version 3 adds an append-only `judgments` relation. An image may have any number of judgments. Every row records `kind`, `producer`, optional `model`, a producer-owned `schema_version`, arbitrary JSON `value`, optional `probability` and `confidence`, JSON-object `metadata`, and `created_at`. The store assigns both an internal sequence and an opaque UUID `judgment_id`.

`value` is deliberately JSON rather than a fixed boolean or enum. This permits independent kinds such as `needs_visual_inspection`, `page_transition_succeeded`, `likely_error_state`, and `visual_change`, while preserving typed booleans, strings, numbers, objects, arrays, and null. `producer` is not constrained to Jev; values such as `rule`, `classical`, `vision-llm`, and `human` use the same relation.

The `judgment add`, `judgment list`, and `judgment search` commands are explicit. `put`, `get`, packing, pruning, and verification never call Jev and never synthesize judgments. Search performs exact canonical-JSON value matching and supports kind, producer, and an exclusive confidence upper bound. Lists and searches use snapshot cursors and the existing bounded-JSON response policy.

The `features` command exposes only locally held inexpensive evidence. It returns source and pixel SHA-256 values, dimensions, source and active-representation sizes, and—when `(run, stream, frame_no)` provides a predecessor—the previous pixel hash and exact pixel identity. It does not materialize an export, decode a temporal segment, invoke OCR, or access a network.

The Jev adapter is an agent workflow in the Visual Store Skill. An MCP-capable host obtains metadata, lightweight features, external observations such as route or console errors, and prior judgments; sends that text/numeric state to the existing `jev-mcp`; then persists the structured answer with `judgment add`. PNG bytes and materialized paths are excluded from the Jev state by default.

## Feature scope

| Classification | Feature | Decision |
| --- | --- | --- |
| Already stored | source SHA-256, pixel SHA-256, width, height, source size, active representation size | Expose through `features`. |
| Cheap lookup | preceding `(run, stream, frame_no)` and pixel-hash equality | Implement without image decode. |
| Higher cost | exact changed-pixel ratio | Defer; it requires decoding both frames and defining channel/alpha semantics. |
| Higher cost | perceptual hash | Defer until an algorithm/version and evaluation corpus are selected. |
| External responsibility | route, action, DOM, console logs, accessibility tree, visible text | The browser or coding agent supplies these to Jev; Visual Store does not collect them. |

An observation relation is intentionally deferred. A future append-only table can mirror the provenance fields used by judgments and hold externally supplied JSON payloads, but this change does not overload `note`, `tags`, or judgment metadata with general observation storage.

## Jev result mapping

- Choice: store the selected option as `value`, the selected option probability as `probability`, the returned confidence as `confidence`, and the full probability distribution in `metadata` when useful.
- Score: store the numeric score as `value`, the returned confidence as `confidence`, and the legend/distribution in `metadata`.
- Noul: store the caller's thresholded boolean as `value` and the returned `noul` value as `probability`. Noul has no separate confidence; do not fabricate one.
- Always store the response model as `model` and use `producer: "jev"`.

The threshold and question definition belong in producer metadata so a future distillation export can distinguish labels created under different policies.

## Vision escalation

External callers search for `needs_visual_inspection == true` or `confidence < threshold`. Only those callers may then run `get` and pass the resulting local path to a vision-capable model. The final vision or human decision can be appended as another judgment without replacing the Jev result. This retains disagreements such as “Jev predicted success, final test failed” for evaluation and later classifier distillation.

## Consequences

Visual Store remains fully usable without Jev, network access, API keys, OCR, browser automation, or a vision model. The v2-to-v3 migration is additive and uses the same backup, journal, resume, and restore protocol as the preceding format migration. Version-2 stores remain usable for existing image operations; judgment commands require explicit migration to version 3.
