-- pg-queue: teardown
-- Removes core infrastructure tables. Queue tables must be dropped individually.

-- Usage: SELECT pg_queue_drop_queue('my_jobs');

CREATE OR REPLACE FUNCTION pg_queue_drop_queue(queue_name TEXT) RETURNS void AS $$
DECLARE
    table_name TEXT := 'queue_' || queue_name;
    func_name  TEXT := 'notify_queue_' || queue_name;
    trig_name  TEXT := 'queue_' || queue_name || '_notify';
BEGIN
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', trig_name, table_name);
    EXECUTE format('DROP FUNCTION IF EXISTS %I()', func_name);
    EXECUTE format('DROP TABLE IF EXISTS %I', table_name);
END;
$$ LANGUAGE plpgsql;

-- Drop request-response infrastructure
DROP TRIGGER IF EXISTS response_notify ON request_responses;
DROP FUNCTION IF EXISTS notify_response();
DROP TABLE IF EXISTS request_responses;

-- Drop cache
DROP TABLE IF EXISTS cache_entries;

-- Drop the queue creator itself
DROP FUNCTION IF EXISTS pg_queue_create_queue(TEXT);
DROP FUNCTION IF EXISTS pg_queue_drop_queue(TEXT);
