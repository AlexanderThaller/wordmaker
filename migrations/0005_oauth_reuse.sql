-- Refresh tokens are kept after use, until they expire, so that a stolen
-- one presented again is recognised and its sign-in revoked.
ALTER TABLE oauth_tokens ADD COLUMN used INTEGER NOT NULL DEFAULT 0;

-- Likewise for codes: a redeemed code keeps the grant it was redeemed for
-- until it expires, so that a replay can revoke that grant.
ALTER TABLE oauth_codes ADD COLUMN grant_id TEXT;
