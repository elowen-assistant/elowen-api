create table if not exists device_trust_events (
    id text primary key,
    device_id text not null,
    event_type text not null,
    actor_username text,
    actor_display_name text,
    actor_role text,
    reason text,
    previous_status text,
    next_status text,
    edge_public_key text,
    previous_edge_public_key text,
    orchestrator_key_id text,
    orchestrator_public_key text,
    payload_json jsonb not null default '{}'::jsonb,
    created_at timestamptz not null default now()
);

create index if not exists idx_device_trust_events_device_created
    on device_trust_events(device_id, created_at desc);

create table if not exists orchestrator_signer_states (
    public_key text primary key,
    key_id text not null unique,
    status text not null,
    actor_username text,
    actor_display_name text,
    actor_role text,
    reason text,
    staged_at timestamptz,
    activated_at timestamptz,
    retired_at timestamptz,
    updated_at timestamptz not null default now(),
    created_at timestamptz not null default now()
);

create index if not exists idx_orchestrator_signer_states_status
    on orchestrator_signer_states(status);
