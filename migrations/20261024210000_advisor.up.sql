-- The advisor's own state, in its own schema: conversations, the suggestions
-- an admin approves or rejects, and the scheduled digests. Nothing here is
-- FalconGo data — approved changes are applied through FalconGo's API, which
-- records them in its ai_audit_logs.

CREATE SCHEMA IF NOT EXISTS advisor;

CREATE TABLE advisor.conversations (
    id          UUID PRIMARY KEY,
    user_id     BIGINT NOT NULL,
    title       TEXT NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at  TIMESTAMPTZ
);
CREATE INDEX conversations_user_idx ON advisor.conversations (user_id, updated_at DESC)
    WHERE deleted_at IS NULL;

-- The transport transcript, one row per message, replayed verbatim on the next
-- turn (tool calls and results included).
CREATE TABLE advisor.messages (
    id               BIGSERIAL PRIMARY KEY,
    conversation_id  UUID NOT NULL REFERENCES advisor.conversations(id) ON DELETE CASCADE,
    message          JSONB NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX messages_conversation_idx ON advisor.messages (conversation_id, id);

CREATE TABLE advisor.suggestions (
    id                   UUID PRIMARY KEY,
    conversation_id      UUID REFERENCES advisor.conversations(id) ON DELETE SET NULL,
    kind                 TEXT NOT NULL,
    title                TEXT NOT NULL,
    rationale            TEXT NOT NULL,
    -- What the reviewer sees changing: the current values and the new ones.
    before               JSONB NOT NULL DEFAULT '{}'::jsonb,
    after                JSONB NOT NULL DEFAULT '{}'::jsonb,
    target               TEXT NOT NULL CHECK (target IN ('falcon', 'maint')),
    method               TEXT NOT NULL,
    path                 TEXT NOT NULL,
    body                 JSONB NOT NULL,
    status               TEXT NOT NULL DEFAULT 'pending'
                         CHECK (status IN ('pending', 'applying', 'applied', 'failed', 'rejected')),
    proposed_for_user_id BIGINT NOT NULL,
    decided_by_user_id   BIGINT,
    decided_at           TIMESTAMPTZ,
    applied_at           TIMESTAMPTZ,
    result_status        INT,
    result               JSONB,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX suggestions_status_idx ON advisor.suggestions (status, created_at DESC);

CREATE TABLE advisor.digests (
    id            UUID PRIMARY KEY,
    period_start  DATE NOT NULL,
    period_end    DATE NOT NULL,
    body          TEXT NOT NULL,
    model         TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (period_start, period_end)
);
