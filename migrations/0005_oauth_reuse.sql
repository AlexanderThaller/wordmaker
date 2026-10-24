-- Refresh tokens are kept after use, until they expire, so that a stolen
-- one presented again is recognised and its sign-in revoked.
ALTER TABLE oauth_tokens ADD COLUMN used INTEGER NOT NULL DEFAULT 0;
