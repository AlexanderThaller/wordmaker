-- Refresh tokens are kept after use, until they expire, so that a stolen
-- one presented again is recognised and its sign-in revoked.
ALTER TABLE oauth_tokens ADD COLUMN used INTEGER NOT NULL DEFAULT 0;

-- Likewise for codes: a redeemed code keeps the grant it was redeemed for
-- until it expires, so that a replay can revoke that grant.
ALTER TABLE oauth_codes ADD COLUMN grant_id TEXT;

-- Whether the authorization request named its redirect_uri, in which case
-- the token request has to repeat it.
ALTER TABLE oauth_codes ADD COLUMN redirect_uri_sent INTEGER NOT NULL DEFAULT 1;

-- When a registered client first got tokens. Clients that never did can
-- be dropped to make room for new registrations.
ALTER TABLE oauth_clients ADD COLUMN used_at TEXT;
UPDATE oauth_clients SET used_at = created_at
WHERE client_id IN (SELECT client_id FROM oauth_tokens);
