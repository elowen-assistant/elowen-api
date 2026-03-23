alter table jobs
    add column if not exists correlation_id text;

update jobs
set correlation_id = coalesce(nullif(correlation_id, ''), id)
where correlation_id is null
   or correlation_id = '';

alter table jobs
    alter column correlation_id set not null;

create index if not exists idx_jobs_correlation_id on jobs(correlation_id);

alter table job_events
    add column if not exists correlation_id text;

update job_events
set correlation_id = jobs.correlation_id
from jobs
where jobs.id = job_events.job_id
  and (job_events.correlation_id is null or job_events.correlation_id = '');

alter table job_events
    alter column correlation_id set not null;

create index if not exists idx_job_events_correlation_id on job_events(correlation_id);
