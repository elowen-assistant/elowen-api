create table if not exists threads (
    id text primary key,
    title text not null,
    status text not null,
    current_summary_id text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table if not exists messages (
    id text primary key,
    thread_id text not null references threads(id) on delete cascade,
    role text not null,
    content text not null,
    status text not null default 'committed',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists idx_messages_thread_id on messages(thread_id);
