---
name: dvb-migration
description: How to add a store migration and check upgrades. Use when changing the SQLite schema in dvb-store.
---

# Adding a migration

Migrations live in `crates/dvb-store` (refinery, ordered, checksummed, one
transaction each). They are add-only: never edit or delete an applied migration.

1. Add the next numbered SQL file; never change an earlier one.
2. Check upgrades through the CI upgrade job (`scripts/run-upgrade.sh`): seed data
   on the previous release, then verify it survives installation and self-update.
