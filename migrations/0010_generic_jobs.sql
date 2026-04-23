alter table jobs
    add column if not exists target_kind text not null default 'repository',
    add column if not exists capability_name text;

alter table jobs
    alter column repo_name drop not null;

update jobs
set target_kind = 'repository'
where target_kind is null or trim(target_kind) = '';

create index if not exists idx_jobs_target_kind on jobs(target_kind);
create index if not exists idx_jobs_capability_name on jobs(capability_name);
