// Portions of this file are ported from ripsecrets 0.1.11.
// Copyright 2022 Brian Smith. Licensed under the MIT License.
// The complete notice is included in the vaultr LICENSE file.

#[derive(Clone, Copy)]
pub(crate) struct PatternSpec {
    pub(crate) id: &'static str,
    pub(crate) expression: &'static str,
    pub(crate) secret_group: Option<usize>,
}

// These are the predefined patterns from ripsecrets 0.1.11. Keep their
// expressions unchanged so the native scanner can be compared with the old
// gate one rule at a time.
pub(crate) const RIPSECRETS_PATTERNS: &[PatternSpec] = &[
    PatternSpec {
        id: "url-credential",
        expression: r"[A-Za-z]+://\S{3,50}:(\S{8,50})@[\dA-Za-z#%&+./:=?_~-]+",
        secret_group: Some(1),
    },
    PatternSpec {
        id: "jwt",
        expression: r"\beyJ[\dA-Za-z=_-]+(?:\.[\dA-Za-z=_-]{3,}){1,4}",
        secret_group: None,
    },
    PatternSpec {
        id: "github-token",
        expression: r"(?:gh[oprsu]|github_pat)_[\dA-Za-z_]{36}",
        secret_group: None,
    },
    PatternSpec {
        id: "gitlab-token",
        expression: r"glpat-[\dA-Za-z_=-]{20,22}",
        secret_group: None,
    },
    PatternSpec {
        id: "stripe-live-key",
        expression: r"[rs]k_live_[\dA-Za-z]{24,247}",
        secret_group: None,
    },
    PatternSpec {
        id: "square-oauth-token",
        expression: r"sq0i[a-z]{2}-[\dA-Za-z_-]{22,43}",
        secret_group: None,
    },
    PatternSpec {
        id: "square-access-token",
        expression: r"sq0c[a-z]{2}-[\dA-Za-z_-]{40,50}",
        secret_group: None,
    },
    PatternSpec {
        id: "square-application-token",
        expression: r"EAAA[\dA-Za-z+=-]{60}",
        secret_group: None,
    },
    PatternSpec {
        id: "azure-account-key",
        expression: r"AccountKey=[\d+/=A-Za-z]{88}",
        secret_group: None,
    },
    PatternSpec {
        id: "gcp-api-key",
        expression: r"AIzaSy[\dA-Za-z_-]{33}",
        secret_group: None,
    },
    PatternSpec {
        id: "npm-token",
        expression: r"npm_[\dA-Za-z]{36}",
        secret_group: None,
    },
    PatternSpec {
        id: "npm-auth-token",
        expression: r"//.+/:_authToken=[\dA-Za-z_-]+",
        secret_group: None,
    },
    PatternSpec {
        id: "slack-token",
        expression: r"xox[aboprs]-(?:\d+-)+[\da-z]+",
        secret_group: None,
    },
    PatternSpec {
        id: "maven-secret",
        expression: r"<master>\{[\dA-Za-z]+\\}</master>",
        secret_group: None,
    },
    PatternSpec {
        id: "slack-webhook",
        expression: r"https://hooks\.slack\.com/services/T[\dA-Za-z_]+/B[\dA-Za-z_]+/[\dA-Za-z_]+",
        secret_group: None,
    },
    PatternSpec {
        id: "sendgrid-api-key",
        expression: r"SG\.[\dA-Za-z_-]{22}\.[\dA-Za-z_-]{43}",
        secret_group: None,
    },
    PatternSpec {
        id: "twilio-api-key",
        expression: r"(?:AC|SK)[\da-z]{32}",
        secret_group: None,
    },
    PatternSpec {
        id: "mailchimp-api-key",
        expression: r"[\da-f]{32}-us\d{1,2}",
        secret_group: None,
    },
    PatternSpec {
        id: "intra42-token",
        expression: r"s-s4t2(?:af|ud)-[\da-f]{64}",
        secret_group: None,
    },
    PatternSpec {
        id: "putty-private-key",
        expression: "PuTTY-User-Key-File-2",
        secret_group: None,
    },
    PatternSpec {
        id: "age-secret-key",
        expression: r"AGE-SECRET-KEY-1[\dA-Z]{58}",
        secret_group: None,
    },
    PatternSpec {
        id: "dsa-private-key",
        expression: r"-{5}BEGIN DSA PRIVATE KEY-{5}(?:$|[^-]{63,}-{5}END)",
        secret_group: None,
    },
    PatternSpec {
        id: "ec-private-key",
        expression: r"-{5}BEGIN EC PRIVATE KEY-{5}(?:$|[^-]{63,}-{5}END)",
        secret_group: None,
    },
    PatternSpec {
        id: "openssh-private-key",
        expression: r"-{5}BEGIN OPENSSH PRIVATE KEY-{5}(?:$|[^-]{63,}-{5}END)",
        secret_group: None,
    },
    PatternSpec {
        id: "pgp-private-key",
        expression: r"-{5}BEGIN PGP PRIVATE KEY BLOCK-{5}(?:$|[^-]{63,}-{5}END)",
        secret_group: None,
    },
    PatternSpec {
        id: "private-key",
        expression: r"-{5}BEGIN PRIVATE KEY-{5}(?:$|[^-]{63,}-{5}END)",
        secret_group: None,
    },
    PatternSpec {
        id: "rsa-private-key",
        expression: r"-{5}BEGIN RSA PRIVATE KEY-{5}(?:$|[^-]{63,}-{5}END)",
        secret_group: None,
    },
    PatternSpec {
        id: "ssh2-private-key",
        expression: r"-{5}BEGIN SSH2 ENCRYPTED PRIVATE KEY-{5}(?:$|[^-]{63,}-{5}END)",
        secret_group: None,
    },
];

// Plant-specific rules. The first two retain the measured tuning from the
// original seal scrub. The delimiter capture keeps JSON structure intact.
pub(crate) const PLANT_PATTERNS: &[PatternSpec] = &[
    PatternSpec {
        id: "anthropic-key",
        expression: r"sk-ant-[A-Za-z0-9_-]{20,}",
        secret_group: None,
    },
    PatternSpec {
        id: "openai-key",
        // LiteLLM / OpenAI. The leading delimiter is load-bearing, not decor:
        // inside a base64 run `sk-` is always preceded by a base64 byte, so
        // demanding a non-base64 byte in front took this from 1254 of 3182
        // committed seals down to 28, and every >=130-byte over-match to zero.
        // It is a capture group because the replacement puts it back. It is
        // often the quote closing a JSON field name.
        // ponytail: the {,63} ceiling is what measured clean. A longer key
        // redacts its first 63 bytes and leaks the tail. Raise it only with
        // the same over-match measurement behind it.
        expression: r"(^|[^A-Za-z0-9_/+-])(sk-[A-Za-z0-9_-]{20,63})",
        secret_group: Some(2),
    },
    PatternSpec {
        id: "gitlab-pat",
        expression: r"glpat-[A-Za-z0-9_-]{20,}",
        secret_group: None,
    },
    PatternSpec {
        id: "gitlab-runner-token",
        expression: r"glrt-[A-Za-z0-9_-]{20,}",
        secret_group: None,
    },
    PatternSpec {
        id: "aws-access-key-id",
        expression: r"(?:AKIA|ASIA)[0-9A-Z]{16}",
        secret_group: None,
    },
    PatternSpec {
        id: "aws-secret-access-key",
        // The 40-char AWS secret is keyed on the field name so it cannot
        // over-match base64. Nothing else catches this shape. The pre-push
        // gate never reads a seal, and detect-aws-credentials only matches
        // keys already in ~/.aws/credentials, which is empty on this host.
        expression: r#"(?i)aws_secret_access_key["']?\s*[:=]\s*["']?[A-Za-z0-9+/]{40}"#,
        secret_group: None,
    },
    PatternSpec {
        id: "github-token-plant",
        expression: r"gh[posru]_[A-Za-z0-9]{36,}",
        secret_group: None,
    },
    PatternSpec {
        id: "slack-token-plant",
        expression: r"xox[baprs]-[A-Za-z0-9-]{10,}",
        secret_group: None,
    },
    PatternSpec {
        id: "slack-webhook-plant",
        expression: r"https://hooks\.slack\.com/services/[A-Za-z0-9/]+",
        secret_group: None,
    },
    PatternSpec {
        id: "google-api-key-plant",
        expression: r"AIza[0-9A-Za-z_-]{35}",
        secret_group: None,
    },
    PatternSpec {
        id: "google-oauth-token",
        expression: r"ya29\.[0-9A-Za-z_-]{20,}",
        secret_group: None,
    },
    PatternSpec {
        id: "private-key-plant",
        expression: r"(?s)-----BEGIN[A-Z ]*PRIVATE KEY-----.*?-----END[A-Z ]*PRIVATE KEY-----",
        secret_group: None,
    },
];
