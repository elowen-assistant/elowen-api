create table if not exists devices (
    id text primary key,
    name text not null,
    primary_flag boolean not null default false,
    metadata jsonb not null default '{}'::jsonb,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists idx_devices_primary_flag on devices(primary_flag);
