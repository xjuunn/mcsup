ALTER TABLE workspaces RENAME COLUMN is_default TO is_current;

DROP INDEX workspaces_one_default;

CREATE UNIQUE INDEX workspaces_one_current
    ON workspaces (is_current) WHERE is_current = 1;
