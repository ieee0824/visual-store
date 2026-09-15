# Compatibility fixtures

`v1-store/` is an immutable closed-store snapshot created by Visual Store 0.1.0.
Tests copy it to a temporary directory before opening it. Do not regenerate it as
part of ordinary test runs; retain it across dependency updates.
