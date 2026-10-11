-- V1.1: forward evaluation, observation gaps and execution-quality evidence (append-only).

create table if not exists observation_gaps (
  id bigserial primary key,
  profile_key text not null,
  run_id text not null,
  start_us bigint not null,
  end_us bigint,
  state text not null,
  reason text not null,
  doc jsonb not null
);

create table if not exists forward_outcomes (
  id bigserial primary key,
  profile_key text not null,
  eval_id text not null,
  mint text not null,
  path text not null,
  decision text not null,
  horizon_s int not null,
  t0_us bigint not null,
  net_return double precision,
  doc jsonb not null,
  unique (profile_key, eval_id, horizon_s)
);
create index if not exists forward_outcomes_decision on forward_outcomes (profile_key, decision, horizon_s);

create table if not exists execution_checks (
  id bigserial primary key,
  profile_key text not null,
  order_id text not null,
  book_id text not null,
  outcome text not null,
  adverse_bps double precision,
  doc jsonb not null,
  unique (order_id)
);

do $$
declare t text;
begin
  foreach t in array array['observation_gaps','forward_outcomes','execution_checks'] loop
    execute format('drop trigger if exists %I_immutable on %I', t, t);
    execute format('create trigger %I_immutable before update or delete on %I for each row execute function reject_mutation()', t, t);
  end loop;
end $$;

insert into schema_migrations (version, applied_us) values (2, (extract(epoch from now()) * 1000000)::bigint)
  on conflict do nothing;
