alter table jobs
    add column if not exists current_summary_id text,
    add column if not exists execution_report_json jsonb not null default '{}'::jsonb;

create table if not exists summaries (
    id text primary key,
    scope text not null,
    source_id text not null,
    version integer not null,
    content text not null,
    created_at timestamptz not null default now()
);

create unique index if not exists idx_summaries_scope_source_version
    on summaries(scope, source_id, version);

create index if not exists idx_summaries_scope_source_created_at
    on summaries(scope, source_id, created_at desc);

create table if not exists approvals (
    id text primary key,
    thread_id text not null references threads(id) on delete cascade,
    job_id text not null references jobs(id) on delete cascade,
    action_type text not null,
    status text not null,
    summary text not null,
    resolved_by text,
    resolution_reason text,
    created_at timestamptz not null default now(),
    resolved_at timestamptz,
    updated_at timestamptz not null default now()
);

create index if not exists idx_approvals_thread_id on approvals(thread_id);
create index if not exists idx_approvals_job_id on approvals(job_id);
create unique index if not exists idx_approvals_pending_push
    on approvals(job_id, action_type)
    where status = 'pending';
