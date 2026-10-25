-- The event log (ADR-0003, contracts.md §2): append-only. Projections are rebuilt
-- from it; S1 adds their tables in later migrations.
CREATE TABLE events (
    id       INTEGER PRIMARY KEY,
    at       TEXT    NOT NULL,
    actor    TEXT    NOT NULL,
    project  TEXT    NULL,
    kind     TEXT    NOT NULL,
    payload  TEXT    NOT NULL,
    evidence TEXT    NULL
);
