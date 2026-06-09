#!/usr/bin/env bash
# Security-invariant gate: key generation and direct RNG-API calls are confined
# to the audit signer (06 §6 author-channel carve-out). The writer channel stays
# verify-only (06 §3) — a host's private key never enters the substrate — and
# Cargo feature unification compiles ed25519-dalek's `rand_core` keygen into
# every crate that links it, so "keygen is off" is a call-level guarantee.
# This gate is that guarantee. It greps identifiers, so indirect entropy
# consumers it cannot see (uuid's v7 id minting) are out of scope by design.
#
# Allowed caller: crates/aionforge-trust/src/audit_signer.rs (AuditSigner::mint).
# Runs from repo root. macOS bash 3.x compatible.

set -euo pipefail

PATTERN='\bOsRng\b|SigningKey::generate|\bthread_rng\b|\bfrom_entropy\b|\bgetrandom\b'
violations=0

while IFS= read -r f; do
  case "$f" in
    crates/aionforge-trust/src/audit_signer.rs) continue ;;
  esac
  if grep -nE "$PATTERN" "$f" >/dev/null 2>&1; then
    echo "FAIL: $f reaches for key generation / a direct RNG API (allowed only in audit_signer.rs):"
    grep -nE "$PATTERN" "$f" || true
    violations=$((violations + 1))
  fi
done < <(git ls-files '*.rs' 2>/dev/null || true)

if [ "$violations" -gt 0 ]; then
  echo
  echo "Mint keys only through AuditSigner::mint (06 §6); the writer channel never generates keys (06 §3)."
  exit 1
fi

echo "OK: key generation and direct RNG-API calls are confined to audit_signer.rs."
