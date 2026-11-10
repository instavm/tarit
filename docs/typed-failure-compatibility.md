# Failure classifications

Share allocation retries `ShareCollision` (slug or share-ID uniqueness), not
`ShareTargetChanged` or arbitrary database conflicts. SQLite classifies UNIQUE
and PRIMARYKEY failures; PostgreSQL classifies UNIQUE_VIOLATION only at the share
insert boundary. Other PostgreSQL FK/CHECK conflicts still return HTTP 409, but
no longer consume the eight slug attempts before returning. Target changes keep
the existing conflict message and HTTP 409.

The scheduler reports `HostCapacity` for VM/vCPU/memory reservation limits. It
keeps HTTP 429, the existing message, and Retry-After: 1. API local admission and
on-demand warm restore use the variant to select capacity reclaim/retry; other
overloads retain their existing handling. Peer HTTP 429 remains a general
retryable overload; this change adds no HTTP wire field.

VMM JSON error responses retain `status: "err"` and `msg`, with an optional
`code: "reflink_required"` when an atomic live disk capture cannot satisfy its
reflink requirement. Error text, cleanup, and paused-snapshot fallback stay the
same. This class covers every failure of required FICLONE, as the previous
message-based fallback did; it does not assert every errno means unsupported.

Old clients ignore the additional JSON field and continue using the unchanged
message. New clients accept old responses without a code and recognize the
legacy reflink message only at `classify_api_error`; the existing `retryable: `
prefix takes precedence there. Explicit codes select behavior independently of
wording. Unknown future codes decode as ordinary errors (while preserving the
existing retryable prefix behavior). Rebuilding downstream Rust consumers of the
public error/response enums requires handling the added variants/field.

This is bounded control-flow hardening. No current production failure caused by
message wording was reproduced, and unrelated message parsing is unchanged.
