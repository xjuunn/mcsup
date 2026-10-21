DROP INDEX workspaces_one_current;

ALTER TABLE workspaces RENAME COLUMN is_current TO is_default;

CREATE UNIQUE INDEX workspaces_one_default
    ON workspaces (is_default) WHERE is_default = 1;
