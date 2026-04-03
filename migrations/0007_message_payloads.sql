alter table messages
    add column if not exists payload_json jsonb not null default '{}'::jsonb;
