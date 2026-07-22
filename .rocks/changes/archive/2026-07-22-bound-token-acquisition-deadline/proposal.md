# Bound token acquisition deadline independently of the OTLP request deadline

## Problem

Plant's OTel export silently dies for a full day at a time. On 2026-07-22 the
`vaultr-usage` Grafana dashboard flatlined ~06:00 while 28 sessions were live;
`~/.local/state/plant/launchd.log` held 209 `[otel] cnb auth token failed /
timed out; skipping flush` lines. Capture was healthy — only the metrics/logs
push was stalled. It only recovered when an interactive `cnb auth token` was run
by hand, outside Plant.

## Root cause (proven)

`Otel::flush` shells out to `cnb auth token` for the OTLP bearer on every 60s
flush, and bounds that subprocess with the **same** `self.timeout` used for the
OTLP HTTP request — `DEFAULT_TIMEOUT` = 5s (`crates/plant/src/otel.rs`, token
subprocess at the `cnb auth token` call, HTTP request in `export`). The token is
short-lived (~24h). When it expires, `cnb auth token` performs an OIDC refresh
round-trip; under the launchd agent context that exceeds 5s. Plant's
`kill_on_drop` kills the child at the 5s deadline **before cnb writes the
refreshed token to its on-disk cache**. The next flush therefore re-reads the
same expired token and repeats the timeout — a self-perpetuating stall that no
scheduled flush can clear, because clearing it requires the very refresh that
keeps getting killed.

A warm-cache `cnb auth token` returns in ~0.12s, so the 5s budget is fine in
steady state; it is only wrong at the once-per-day refresh moment, where it is
exactly too short to ever succeed.

## Fix

Give token acquisition its own deadline, independent of and larger than the
per-request OTLP deadline, so a cold credential refresh can complete and
repopulate cnb's cache. New field `token_timeout`, env override
`VAULTR_OTEL_TOKEN_TIMEOUT_MS`, default 30s. The OTLP request deadline
(`VAULTR_OTEL_TIMEOUT_MS`, 5s) is unchanged. A cold refresh now blocks one
background flush for up to 30s once per day instead of stalling every flush
forever; proxy traffic is unaffected (the flush loop is a detached task).

This is the source fix for the operational mitigation already deployed
(`vault/jobs/refresh-cnb-token.15m.sh`, which keeps the cache warm from a
process with a generous deadline). With this change the job becomes belt-and-
suspenders rather than load-bearing.
