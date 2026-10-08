-- OAuth for the MCP endpoint. Codes and tokens are stored hashed.

-- Clients registered through dynamic client registration. Clients that use
-- a client ID metadata document are not stored.
CREATE TABLE oauth_clients (
    client_id     TEXT PRIMARY KEY,
    client_name   TEXT,
    redirect_uris TEXT NOT NULL, -- JSON array
    created_at    TEXT NOT NULL
);

CREATE TABLE oauth_codes (
    code_hash      TEXT PRIMARY KEY,
    client_id      TEXT NOT NULL,
    redirect_uri   TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    resource       TEXT NOT NULL,
    expires_at     TEXT NOT NULL
);

-- The access and refresh token of one sign-in share a grant_id, so they
-- can be revoked together.
CREATE TABLE oauth_tokens (
    token_hash TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('access', 'refresh')),
    grant_id   TEXT NOT NULL,
    client_id  TEXT NOT NULL,
    resource   TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE INDEX oauth_tokens_grant_id ON oauth_tokens (grant_id);
