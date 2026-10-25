#!/usr/bin/env bash
# Regenerate the contract files from the engine router and core types: contracts/openapi.json and the
# TypeScript types in ui/src/api/, provider schema and wording. CI checks drift.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

cargo run --locked --quiet -p dvb-engine --bin openapi > contracts/openapi.json
cargo run --locked --quiet -p dvb-core --bin verdict-schema > prompts/verdict.schema.json
cargo run --locked --quiet -p dvb-core --bin wording > contracts/wording.json
cp contracts/wording.json ui/src/api/wording.json
# Pinned and run by npx: openapi-typescript 7 wants TypeScript 5 as a peer, the app uses 7.
npx --yes openapi-typescript@7.13.0 contracts/openapi.json -o ui/src/api/schema.ts --silent
(cd ui && npx --yes "prettier@$(node -p 'require("./package.json").devDependencies.prettier')" --write src/api/schema.ts)
