create table if not exists ui_sessions (
    token text primary key,
    operator_label text not null,
    created_at timestamptz not null default now(),
    expires_at timestamptz not null
);

create index if not exists idx_ui_sessions_expires_at on ui_sessions(expires_at);
