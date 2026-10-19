CREATE TABLE java_runtimes (
    id         TEXT    PRIMARY KEY NOT NULL,
    path       TEXT    NOT NULL UNIQUE,
    major      BIGINT  NOT NULL,
    version    TEXT    NOT NULL,
    vendor     TEXT    NOT NULL,
    managed    INTEGER NOT NULL DEFAULT 0,
    created_at BIGINT  NOT NULL,
    updated_at BIGINT  NOT NULL
);

CREATE TABLE java_scan_state (
    key        TEXT   PRIMARY KEY NOT NULL,
    scanned_at BIGINT NOT NULL
);
