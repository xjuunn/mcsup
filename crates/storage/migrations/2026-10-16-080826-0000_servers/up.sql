CREATE TABLE servers (
    name          TEXT    PRIMARY KEY NOT NULL,
    dir           TEXT    NOT NULL UNIQUE,
    version       TEXT    NOT NULL,
    variant       TEXT    NOT NULL,
    jar_path      TEXT,
    jar_size      BIGINT,
    checksum_algo TEXT,
    checksum      TEXT,
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL
);