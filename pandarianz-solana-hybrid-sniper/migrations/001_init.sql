-- pandarianz-solana-hybrid-sniper schema (PAPER ONLY).
-- Journal / evaluation / order / ledger tables are append-only: UPDATE and DELETE are rejected by trigger.
-- `positions` is the only mutable table (current snapshot, versioned); its history is in position_events.

create table if not exists schema_migrations (version int primary key, applied_us bigint not null);

create table if not exists runs (
  run_id text primary key,
  profile_key text not null,
  data_mode text not null,
  started_us bigint not null,
  strategy_version text not null,
  config jsonb not null
);

create table if not exists raw_events (
  seq bigserial,
  profile_key text not null,
  signature text not null,
  run_id text not null,
  data_mode text not null,
  provider text not null,
  slot bigint not null,
  commitment text not null,
  recv_us bigint not null,
  doc jsonb not null,
  primary key (profile_key, signature)
);
create index if not exists raw_events_recv on raw_events (profile_key, recv_us);

create table if not exists evaluations (
  eval_id text primary key,
  run_id text not null,
  profile_key text not null,
  data_mode text not null,
  ts_us bigint not null,
  mint text not null,
  path text not null,
  decision text not null,
  strategy_version text not null,
  score double precision not null,
  reasons jsonb not null,
  doc jsonb not null
);
create index if not exists evaluations_ts on evaluations (profile_key, ts_us);

create table if not exists books (
  book_id text primary key,
  profile_key text not null,
  scenario text not null,
  start_balance bigint not null,
  created_us bigint not null,
  doc jsonb not null
);

create table if not exists order_submissions (
  order_id text primary key,
  idempotency_key text not null unique,
  profile_key text not null,
  book_id text not null,
  ts_us bigint not null,
  doc jsonb not null
);

create table if not exists orders (
  order_id text primary key,
  idempotency_key text not null unique,
  profile_key text not null,
  book_id text not null,
  kind text not null,
  status text not null,
  mint text not null,
  ts_us bigint not null,
  strategy_version text not null,
  doc jsonb not null
);
create index if not exists orders_book on orders (book_id, ts_us);

create table if not exists ledger (
  book_id text not null,
  seq bigint not null,
  ts_us bigint not null,
  kind text not null,
  amount bigint not null,
  balance_after bigint not null,
  ref_id text not null,
  primary key (book_id, seq)
);

create table if not exists positions (
  position_id text primary key,
  book_id text not null,
  profile_key text not null,
  mint text not null,
  status text not null,
  version bigint not null,
  updated_us bigint not null,
  doc jsonb not null
);
create index if not exists positions_book on positions (book_id, status);

create table if not exists position_events (
  id bigserial primary key,
  position_id text not null,
  ts_us bigint not null,
  kind text not null,
  doc jsonb not null
);

create table if not exists route_checks (
  id bigserial primary key,
  profile_key text not null,
  ts_us bigint not null,
  mint text not null,
  side text not null,
  source text not null,
  ok boolean not null,
  latency_us bigint not null,
  error text,
  doc jsonb not null
);

create or replace function reject_mutation() returns trigger as $$
begin
  raise exception 'table % is append-only (immutable record)', tg_table_name;
end;
$$ language plpgsql;

do $$
declare t text;
begin
  foreach t in array array['raw_events','evaluations','books','order_submissions','orders','ledger','position_events','route_checks','runs'] loop
    execute format('drop trigger if exists %I_immutable on %I', t, t);
    execute format('create trigger %I_immutable before update or delete on %I for each row execute function reject_mutation()', t, t);
  end loop;
end $$;

insert into schema_migrations (version, applied_us) values (1, (extract(epoch from now()) * 1000000)::bigint)
  on conflict do nothing;
