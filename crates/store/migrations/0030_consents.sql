-- A sign-in for an MCP client that waits for the user's yes on the consent page. The browser
-- that signed in holds `browser_hash`'s secret in a cookie; the page holds the request's.
CREATE TABLE consents (
    request_hash TEXT PRIMARY KEY,
    browser_hash TEXT NOT NULL,
    -- The session a yes hands over. Deleted with the request if nobody answers.
    token_hash TEXT NOT NULL,
    token_nonce BYTEA NOT NULL,
    token_ciphertext BYTEA NOT NULL,
    -- Who asks, and where the answer goes: the server's own JSON.
    client TEXT NOT NULL,
    -- The GitHub login signed in.
    login TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);
