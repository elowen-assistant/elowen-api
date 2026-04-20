alter table if exists ui_sessions
    add column if not exists account_username text,
    add column if not exists display_name text,
    add column if not exists role text,
    add column if not exists last_seen_at timestamptz;

update ui_sessions
set account_username = coalesce(account_username, operator_label),
    display_name = coalesce(display_name, operator_label),
    role = coalesce(role, 'admin'),
    last_seen_at = coalesce(last_seen_at, created_at);

alter table if exists approvals
    add column if not exists resolved_by_display_name text;

