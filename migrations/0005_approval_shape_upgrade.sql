alter table approvals
    add column if not exists summary text not null default '',
    add column if not exists resolved_by text,
    add column if not exists resolution_reason text,
    add column if not exists updated_at timestamptz not null default now();
