CREATE TABLE workspaces (
    id         TEXT    PRIMARY KEY NOT NULL,
    name       TEXT    NOT NULL UNIQUE,
    path       TEXT    NOT NULL UNIQUE,
    is_default INTEGER NOT NULL DEFAULT 0,
    created_at BIGINT  NOT NULL,
    updated_at BIGINT  NOT NULL
);

CREATE UNIQUE INDEX workspaces_one_default
    ON workspaces (is_default) WHERE is_default = 1;

DROP TABLE servers;

CREATE TABLE servers (
    id            TEXT    PRIMARY KEY NOT NULL,
    name          TEXT    NOT NULL UNIQUE,
    workspace_id  TEXT    NOT NULL REFERENCES workspaces (id),
    rel_dir       TEXT    NOT NULL,
    version       TEXT    NOT NULL,
    variant       TEXT    NOT NULL,
    jar_path      TEXT,
    jar_size      BIGINT,
    checksum_algo TEXT,
    checksum      TEXT,
    created_at    BIGINT  NOT NULL,
    updated_at    BIGINT  NOT NULL,
    UNIQUE (workspace_id, rel_dir)
);
