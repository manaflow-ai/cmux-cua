#!/usr/bin/env bash
set -euo pipefail

if [[ "${ALLOW_LEGACY_TOKEN:-false}" != "true" ]]; then
  echo "::error::Legacy token publishing is disabled. Use trusted publishing." >&2
  exit 1
fi

if [[ "${LEGACY_TOKEN_GATE:-}" != "enabled" ]]; then
  echo "::error::The protected legacy-token environment is not enabled." >&2
  exit 1
fi

if [[ -z "${REGISTRY_TOKEN:-}" ]]; then
  echo "::error::The legacy registry token is missing." >&2
  exit 1
fi
