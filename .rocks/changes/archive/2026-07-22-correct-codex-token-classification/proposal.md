# Correct codex token classification (no cache double-count)

## Problem

On the `vaultr-usage` dashboard, codex (gpt-5.6-sol) token metrics are inflated
and mis-split versus claude. Codex's input-side totals, cache-hit rate, and the
Output/input panel are all skewed.

## Root cause (proven from live capture)

Plant's codex adapter (`crates/plant/src/adapter.rs`, `usage()` Codex branch)
maps OpenAI Responses-API usage as:

- `input      = usage.input_tokens`
- `cache_read = usage.input_tokens_details.cached_tokens`
- `cache_creation = 0` (hardcoded)

But in the Responses API, `input_tokens` is the **total** input, and
`cached_tokens` / `cache_write_tokens` are **subsets of it** — not additive.
Confirmed from a real codex capture (`019f7f59-…`):

| input_tokens | cached_tokens | output_tokens | total_tokens |
|---|---|---|---|
| 24466 | 0 | 216 | 24682 |
| 25296 | 24320 | 18 | 25314 |
| 26773 | 24320 | 138 | 26911 |

`total_tokens == input_tokens + output_tokens` exactly, so `cached_tokens` is
inside `input_tokens`. The current mapping therefore records `input` (25296) **and**
`cache_read` (24320) — double-counting the 24320 cached tokens (~26× over on the
input bucket). Claude is unaffected because its `input_tokens` already excludes
cache, giving non-overlapping buckets.

Second defect: the data carries `input_tokens_details.cache_write_tokens`
(codex cache creation), but the adapter hardcodes `cache_creation: 0`, dropping it.

## Fix

Map codex usage to the same non-overlapping buckets Claude uses, so
`input + cache_read + cache_creation == input_tokens`:

- `cache_read     = cached_tokens`
- `cache_creation = cache_write_tokens`
- `input          = input_tokens − cached_tokens − cache_write_tokens` (saturating)

Existing (correct) codex data cannot be retro-fixed in Thanos; the correction
applies to metrics recorded after deploy.
