-- pg-queue: PostgreSQL job queue, pub/sub, and cache
-- Run this migration to set up the core infrastructure tables.
-- Queue tables are created dynamically via pg_queue_create_queue().

-- ============================================================
-- Dynamic queue table creator
-- Usage: SELECT pg_queue_create_queue('my_jobs');
-- Creates: queue_my_jobs table + pending index + NOTIFY trigger
-- ============================================================

CREATE OR REPLACE FUNCTION pg_queue_create_queue(queue_name TEXT) RETURNS void AS $$
DECLARE
    table_name TEXT := 'queue_' || queue_name;
    index_name TEXT := 'idx_queue_' || queue_name || '_pending';
    func_name  TEXT := 'notify_queue_' || queue_name;
    trig_name  TEXT := 'queue_' || queue_name || '_notify';
BEGIN
    -- Create the queue table
    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I (
            id BIGSERIAL PRIMARY KEY,
            payload JSONB NOT NULL,
            status TEXT NOT NULL DEFAULT ''pending'',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            processed_at TIMESTAMPTZ
        )', table_name
    );

    -- Partial index for fast pending job lookup
    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS %I ON %I(created_at) WHERE status = ''pending''',
        index_name, table_name
    );

    -- NOTIFY trigger function
    EXECUTE format(
        'CREATE OR REPLACE FUNCTION %I() RETURNS TRIGGER AS $fn$
         BEGIN
             PERFORM pg_notify(%L, NEW.id::text);
             RETURN NEW;
         END;
         $fn$ LANGUAGE plpgsql',
        func_name, table_name
    );

    -- Attach trigger
    EXECUTE format(
        'DROP TRIGGER IF EXISTS %I ON %I',
        trig_name, table_name
    );
    EXECUTE format(
        'CREATE TRIGGER %I
            AFTER INSERT ON %I
            FOR EACH ROW EXECUTE FUNCTION %I()',
        trig_name, table_name, func_name
    );
END;
$$ LANGUAGE plpgsql;

-- ============================================================
-- Cache table (UNLOGGED for performance, per-query TTL check)
-- ============================================================

CREATE UNLOGGED TABLE IF NOT EXISTS cache_entries (
    key TEXT PRIMARY KEY,
    value JSONB NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

-- ============================================================
-- Request-Response table for RPC-style operations
-- ============================================================

CREATE TABLE IF NOT EXISTS request_responses (
    request_id UUID PRIMARY KEY,
    response JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE OR REPLACE FUNCTION notify_response() RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify('response_' || NEW.request_id::text, '');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS response_notify ON request_responses;
CREATE TRIGGER response_notify
    AFTER INSERT ON request_responses
    FOR EACH ROW EXECUTE FUNCTION notify_response();
