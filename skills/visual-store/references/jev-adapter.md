# Jev judgment adapter

This workflow uses the separately configured `jev-mcp` MCP server. Visual Store does not configure TypeSafe credentials and does not call Jev itself.

## Boundary

Never place PNG bytes, Base64, data URLs, or a materialized image path in Jev `state`. Use the opaque `visual://` reference, `vstore info`, `vstore features`, prior judgments, and text/numeric observations obtained by the calling agent.

Do not run `vstore get` before the judgment cascade says visual inspection is needed.

## Recommended flow

1. Run `vstore info REF`.
2. Run `vstore features REF`.
3. Run `vstore judgment list REF` and reuse a suitable existing result when its producer/model/schema and context are still applicable.
4. Add external observations already held by the caller, such as action, route, console error count, or visible text. Visual Store does not collect them.
5. Ask atomic questions through `jev.batch` when they share the same state. Example independent IDs are `page_transition_succeeded`, `visual_change`, `likely_error_state`, and `needs_visual_inspection`.
6. Store each returned answer with `vstore judgment add REF --json FILE` or `--stdin`.
7. Run a search for `needs_visual_inspection == true` or low confidence.
8. Only then run `vstore get REF` and pass the returned path to the host's vision capability.
9. Save the vision or human conclusion as another judgment with `producer` set to `vision-llm` or `human`.

Example Jev state:

```json
{
  "action": "login button clicked",
  "previous": {"route": "/login"},
  "current": {
    "route": "/dashboard",
    "console_errors": [],
    "visible_text": ["Dashboard", "Welcome back"],
    "screenshot": "visual://STORE/images/IMAGE",
    "features": {
      "width": 1440,
      "height": 900,
      "same_pixels_as_previous": false
    }
  }
}
```

Example Choice result persisted through stdin:

```bash
printf '%s\n' '{
  "kind":"visual_change",
  "producer":"jev",
  "model":"jev-1.13.0",
  "schema_version":1,
  "value":"unexpected",
  "probability":0.91,
  "confidence":0.87,
  "metadata":{
    "question_type":"choice",
    "probabilities":{"none":0.02,"expected":0.07,"unexpected":0.91}
  }
}' | vstore judgment add 'visual://STORE/images/IMAGE' --stdin
```

For Noul, store the Yes probability in `probability`. It has no separate Jev confidence, so omit `confidence`; if `value` is thresholded to a boolean, record the threshold in metadata.

## Escalation queries

```bash
vstore judgment search \
  --kind needs_visual_inspection \
  --value true

vstore judgment search \
  --producer jev \
  --confidence-below 0.70

vstore judgment search --producer vision-llm
```

Choice and Score confidence describes the returned distribution; it is not a correctness guarantee. Tune thresholds against labeled outcomes rather than treating the examples above as universal policy.
