create table if not exists jobs (
    id text primary key,
    short_id text not null unique,
    title text not null,
    thread_id text not null references threads(id) on delete cascade,
    status text not null,
    result text,
    failure_class text,
    repo_name text not null,
    device_id text references devices(id),
    branch_name text,
    base_branch text,
    parent_job_id text references jobs(id),
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    completed_at timestamptz
);

create table if not exists job_events (
    id text primary key,
    job_id text not null references jobs(id) on delete cascade,
    event_type text not null,
    payload_json jsonb not null default '{}'::jsonb,
    created_at timestamptz not null default now()
);

create index if not exists idx_jobs_thread_id on jobs(thread_id);
create index if not exists idx_jobs_device_id on jobs(device_id);
create index if not exists idx_job_events_job_id on job_events(job_id);
