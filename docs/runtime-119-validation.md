# Direct-authority mint validation

Branch: `feat/runtime-119-direct-mint`, base `62e8a0f`.
Runtime pin: `bea1395381cff989a66bbfd4e4ca06b3c09f937a`.

Implemented: direct signed FaucetOps.mint, finalized exact-extrinsic System and
matching FaucetOps.Minted plus canonical EmissionController.FaucetMinted receipts,
fail-closed receipt handling, an ambiguous-outcome latch, and finalized-block
Authority/State/Budget/Issued checks at startup, refresh and before minting.
Requests/signing consume cached controls; readiness stays valid during refresh
and expires after 12 seconds. Definite nonce rejections retry up to five times;
other definite pre-submission/pool rejections do not latch.
The HTTP health endpoint rejects stale or failed checks. Deployment examples
use a separately appointed authority; the existing env variable is retained.

Pinned-package evidence (2026-09-29):

- Reviewer published runtime `bea1395` and regenerated Cargo.lock. All four
  direct R2 dependencies resolve to that commit; SDK resolves to `845eb998`.
- Reviewer ran `cargo +1.95 build --locked`: passed in 4m56s, including the real
  HTTP/main package and axum 0.7.9 against the pinned runtime dependencies.
- `cargo +1.95 test --locked`: 23 passed, zero failed or ignored.
- `cargo +1.95 clippy --all-targets --locked -- -D warnings`: passed in 2m14s
  without source-harness lint allowances.
- Formatting and staged whitespace checks pass.
- Initial source-harness checks and DNS/cache failures are historical; the
  actual locked package checks above supersede that limited build evidence.
- Pinned-package metadata V16 export and byte-for-byte snapshot comparison pass
  locally; hash and reproduction command are in `metadata/README.md`. CI repeats
  this check.

Still required: published CI/MR checks, the release binary build and live smoke
below. rr approved the code after H1/H2 fixes; the subsequent inclusion-history
edge case is also addressed and tested. No release-build success is inferred
from the debug build result.

Deployment validation remains separate: finalized direct mint on a fresh
runtime-119 network; Foundation key rotation/revocation and service 503 behavior;
fuse/budget rejection; old-key wallet access/fee funding; and finality/upgrade
rehearsals. No deployment or live chain modification was performed.

On ambiguous mint outcomes, reconcile the exact transaction against chain
history before restarting. Already handed-out pool transactions cannot be
revoked by this service. Control checks are preflight guards; on-chain dispatch
remains authoritative for governance changes racing submission.

The watch stream remembers inBlock/retracted status. A later invalid status
remains ambiguous even after intervening ready status; plain invalid without
inclusion history remains a definite rejection.
