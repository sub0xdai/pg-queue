-- Run once per database. Re-running upgrades existing queue tables in place.

CREATE OR REPLACE FUNCTION pg_queue_create_queue(queue_name TEXT) RETURNS void AS $$
DECLARE
    table_name       TEXT := 'queue_' || queue_name;
    ready_index      TEXT := 'idx_queue_' || queue_name || '_ready';
    stale_index      TEXT := 'idx_queue_' || queue_name || '_stale';
    old_index        TEXT := 'idx_queue_' || queue_name || '_pending';
    func_name        TEXT := 'notify_queue_' || queue_name;
    insert_trig      TEXT := 'queue_' || queue_name || '_notify_insert';
    retry_trig       TEXT := 'queue_' || queue_name || '_notify_retry';
    old_trig         TEXT := 'queue_' || queue_name || '_notify';
BEGIN
    -- @waiver TB-17: must stay in step with MAX_QUEUE_NAME_LEN in src/queue.rs.
    -- PostgreSQL truncates identifiers at 63 bytes; 43 is the largest safe name.
    IF length(queue_name) > 43 THEN
        RAISE EXCEPTION
            'queue name % is % characters, maximum is 43; rename the table before upgrading',
            queue_name, length(queue_name);
    END IF;

    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I (
            id BIGSERIAL PRIMARY KEY,
            payload JSONB NOT NULL,
            status TEXT NOT NULL DEFAULT ''pending'',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            processed_at TIMESTAMPTZ,
            deliver_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            delivery_attempts INTEGER NOT NULL DEFAULT 0
        )', table_name
    );

    EXECUTE format(
        'ALTER TABLE %I ADD COLUMN IF NOT EXISTS deliver_at TIMESTAMPTZ NOT NULL DEFAULT NOW()',
        table_name
    );
    EXECUTE format(
        'ALTER TABLE %I ADD COLUMN IF NOT EXISTS delivery_attempts INTEGER NOT NULL DEFAULT 0',
        table_name
    );

    EXECUTE format('DROP INDEX IF EXISTS %I', old_index);
    EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %I(deliver_at, created_at)
        WHERE status = ''pending''',
        ready_index, table_name);
    EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %I(processed_at)
        WHERE status = ''processing''',
        stale_index, table_name);

    EXECUTE format(
        'CREATE OR REPLACE FUNCTION %I() RETURNS TRIGGER AS $fn$
         BEGIN
             PERFORM pg_notify(%L, NEW.id::text);
             RETURN NULL;
         END;
         $fn$ LANGUAGE plpgsql',
        func_name, table_name
    );

    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', old_trig, table_name);
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', insert_trig, table_name);
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', retry_trig, table_name);

    EXECUTE format(
        'CREATE TRIGGER %I AFTER INSERT ON %I
            FOR EACH ROW WHEN (NEW.status = ''pending'')
            EXECUTE FUNCTION %I()',
        insert_trig, table_name, func_name
    );

    EXECUTE format(
        'CREATE TRIGGER %I AFTER UPDATE ON %I
            FOR EACH ROW WHEN (NEW.status = ''pending'' AND OLD.status IS DISTINCT FROM ''pending'')
            EXECUTE FUNCTION %I()',
        retry_trig, table_name, func_name
    );
END;
$$ LANGUAGE plpgsql;

-- Only rewrites tables that already look like queue tables.
DO $$
DECLARE rec RECORD;
BEGIN
    FOR rec IN
        SELECT qt.table_name AS name
        FROM information_schema.tables qt
        WHERE qt.table_schema = 'public'
          AND qt.table_name ~ '^queue_'
          AND (
              SELECT count(*) FROM information_schema.columns c
              WHERE c.table_schema = 'public'
                AND c.table_name = qt.table_name
                AND (c.column_name IN ('id', 'payload')
                     OR (c.column_name = 'status' AND c.data_type = 'text'))
          ) = 3
    LOOP
        BEGIN
            PERFORM pg_queue_create_queue(substring(rec.name from 7));
        EXCEPTION WHEN undefined_table THEN
            NULL;
        END;
    END LOOP;
END;
$$;

CREATE UNLOGGED TABLE IF NOT EXISTS cache_entries (
    key TEXT PRIMARY KEY,
    value JSONB NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS request_responses (
    request_id UUID PRIMARY KEY,
    response JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_request_responses_created_at
    ON request_responses (created_at);

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
