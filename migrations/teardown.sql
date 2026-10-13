-- Removes core tables; drop queue tables with pg_queue_drop_queue('my_jobs').

CREATE OR REPLACE FUNCTION pg_queue_drop_queue(queue_name TEXT) RETURNS void AS $$
DECLARE
    table_name  TEXT := 'queue_' || queue_name;
    func_name   TEXT := 'notify_queue_' || queue_name;
    old_trig    TEXT := 'queue_' || queue_name || '_notify';
    insert_trig TEXT := 'queue_' || queue_name || '_notify_insert';
    retry_trig  TEXT := 'queue_' || queue_name || '_notify_retry';
BEGIN
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', old_trig, table_name);
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', insert_trig, table_name);
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', retry_trig, table_name);
    EXECUTE format('DROP FUNCTION IF EXISTS %I()', func_name);
    EXECUTE format('DROP TABLE IF EXISTS %I', table_name);
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS response_notify ON request_responses;
DROP FUNCTION IF EXISTS notify_response();
DROP TABLE IF EXISTS request_responses;

DROP TABLE IF EXISTS cache_entries;

DROP FUNCTION IF EXISTS pg_queue_create_queue(TEXT);
