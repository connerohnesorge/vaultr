// Portions of this file are ported from ripsecrets 0.1.11.
// Copyright 2022 Brian Smith. Licensed under the MIT License.
// The complete notice is included in the vaultr LICENSE file.

use super::p_random::p_random;
use super::patterns::PatternSpec;
use regex::bytes::Regex;
use std::ops::Range;

// We only flag random strings that occur on the same line as one of our four keywords.
pub const RANDOM_STRING_REGEX: &str = r#"(?i:key|token|secret|password)\w*["']?]?\s*(?:[:=]|:=|=>|<-|>)\s*[\t "'`]?([\w+./=~\-\\`^]{15,90})(?:[\t\n "'`]|</|$)"#;

#[derive(Clone, Debug)]
pub(crate) struct CompiledPattern {
    pub(crate) id: &'static str,
    pub(crate) regex: Regex,
    pub(crate) secret_group: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct Detected {
    pub(crate) rule: &'static str,
    pub(crate) full: Range<usize>,
    pub(crate) span: Range<usize>,
}

pub(crate) fn compile(spec: PatternSpec) -> Result<CompiledPattern, regex::Error> {
    Ok(CompiledPattern {
        id: spec.id,
        regex: Regex::new(spec.expression)?,
        secret_group: spec.secret_group,
    })
}

fn capture_span(
    captures: &regex::bytes::Captures<'_>,
    secret_group: Option<usize>,
) -> Option<Range<usize>> {
    match secret_group {
        Some(group) => captures.get(group).map(|m| m.start()..m.end()),
        None => captures.get(0).map(|m| m.start()..m.end()),
    }
}

fn contains_json_escape(candidate: &[u8]) -> bool {
    candidate.windows(2).any(|pair| {
        matches!(
            pair,
            [b'\\', b'n'] | [b'\\', b't'] | [b'\\', b'"'] | [b'\\', b'\\']
        )
    })
}

pub(crate) fn is_random(candidate: &[u8]) -> bool {
    // Serialized capture text can contain a literal `\\n...\\n` sequence after
    // an empty *_KEY= assignment. The backslash is in the upstream character
    // class, but a JSON escape is not a credential.
    if contains_json_escape(candidate) {
        return false;
    }
    let p = p_random(candidate);
    if p < 1.0 / 1e5 {
        return false;
    }
    let mut contains_num = false;
    for b in candidate {
        if *b >= b'0' && *b <= b'9' {
            contains_num = true;
            break;
        }
    }
    if !contains_num && p < 1.0 / 1e4 {
        return false;
    }
    true
}

fn has_allowlist_pragma(haystack: &[u8], start: usize) -> bool {
    const MARKER: &[u8] = b"pragma: allowlist secret";
    let end = haystack[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(haystack.len(), |offset| start + offset);
    haystack[start..end]
        .windows(MARKER.len())
        .any(|window| window == MARKER)
}

pub(crate) fn find(
    haystack: &[u8],
    patterns: &[CompiledPattern],
    ignored_secrets: &std::collections::HashSet<Vec<u8>>,
) -> Vec<Detected> {
    let mut candidates = Vec::new();
    for (pattern_index, pattern) in patterns.iter().enumerate() {
        for captures in pattern.regex.captures_iter(haystack) {
            let Some(full) = captures.get(0).map(|m| m.start()..m.end()) else {
                continue;
            };
            let Some(span) = capture_span(&captures, pattern.secret_group) else {
                continue;
            };
            let candidate = &haystack[span.clone()];
            if pattern.id == "random-string" && !is_random(candidate) {
                continue;
            }
            if ignored_secrets.contains(candidate) || has_allowlist_pragma(haystack, span.end) {
                continue;
            }
            candidates.push((
                full.start,
                pattern_index,
                Detected {
                    rule: pattern.id,
                    full,
                    span,
                },
            ));
        }
    }

    // The upstream matcher uses one combined regex. Sort by the same
    // leftmost-first, pattern-order rule, then discard overlapping matches.
    candidates.sort_by_key(|(start, pattern_index, _)| (*start, *pattern_index));
    let mut accepted = Vec::new();
    let mut cursor = 0;
    for (_, _, candidate) in candidates {
        if candidate.full.start < cursor {
            continue;
        }
        cursor = candidate.full.end.max(candidate.span.end);
        accepted.push(candidate);
    }
    accepted.sort_by_key(|candidate| candidate.span.start);
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_candidate_is_not_random() {
        assert!(!is_random(br#"\\n0123456789abcdef\\nset"#));
        assert!(!is_random(br#"\\t0123456789abcdef"#));
    }

    #[test]
    fn random_match_reports_only_the_submatch() {
        let pattern = compile(PatternSpec {
            id: "random-string",
            expression: RANDOM_STRING_REGEX,
            secret_group: Some(1),
        })
        .unwrap();
        let bytes = b"TOKEN=pk_test_TYooMQauvdEDq54NiTphI7jx";
        let matches = find(bytes, &[pattern], &Default::default());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].span, 6..38);
        assert_eq!(
            &bytes[matches[0].span.clone()],
            b"pk_test_TYooMQauvdEDq54NiTphI7jx"
        );
    }
}
