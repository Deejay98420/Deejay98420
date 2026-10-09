"""The desk: CHIEF coordinates deterministic seat workers over one cycle.

  SCAN    discovery (GeckoTerminal new_pools, bounded pages) + watchlist re-quotes (exact pools)
          + pre-dossier arithmetic. No per-token network.
  VET     bounded dossiers: token info, mint parsed/raw agreement, controls, concentration,
          then the valuation gate.
  SOCIAL  exact project-published handle only; adapter disconnected -> evidence gap recorded.
  CHIEF   runs the configured judge (RULES or TYPED_AI), picks one candidate or records why not.
  SIZE    arithmetic only.
  FILLS   fresh exact-pool re-quote, then a simulated paper fill.
  RISK    monitors the one position by exact pool; closes on its own authority.

While a position is held there is no scan at all; RISK keeps running.
These seats are deterministic workers, not separate AI agents.
"""

from __future__ import annotations

import time
from decimal import Decimal

from . import bench, book, filters, sizing, store, valuation, watchlist
from .db import Database
from .execution import FillRefused, get_adapter
from .integrations import TelegramNotifier
from .judges import JudgeSchemaError, JudgeUnavailable, make_judge
from .mint import verify_mint
from .providers import Budget, Http, ProviderError
from .providers.dexscreener import DexScreener
from .providers.geckoterminal import GeckoTerminal
from .providers.solana_rpc import SolanaRpc
from .social import DisconnectedXAdapter, social_evidence
from .store import Lease, LeaseLost, emit, set_seat
from .util import dec, fnum, jdump, now, sanitize, stable_candidate_id

GT, DEX, RPC = "geckoterminal", "dexscreener", "solana_rpc"


class Desk:
    def __init__(self, db: Database, http: Http | None = None, *, rpc_url: str | None = None,
                 social_adapter=None, typesafe_client=None, demo: bool = False, notifier=None):
        self.db = db
        self.http = http or Http()
        self.gt = GeckoTerminal(self.http)
        self.dex = DexScreener(self.http)
        self.rpc = SolanaRpc(self.http, rpc_url)
        self.social = social_adapter or DisconnectedXAdapter()
        self.typesafe_client = typesafe_client
        self.demo = demo
        self.adapter = get_adapter("paper")
        self.notifier = notifier or TelegramNotifier.from_env(self.http)
        self.http.limiter.set_limit(DEX, 60)
        self.http.limiter.set_limit(RPC, 120)
        self.http.limiter.set_limit("typesafe", 600)
        self.http.limiter.set_limit("telegram", 20)

    # ------------------------------------------------------------------ helpers
    def judge_for(self, st: dict):
        return make_judge(st["judge_engine"], self.http, self.typesafe_client)

    def _provider_fail(self, name: str, e: ProviderError, seat: str):
        store.provider_err(self.db, name, e.kind, str(e))
        emit(self.db, seat, "provider_" + e.kind, f"{name} {e.kind}: {e}", level="warn",
             data={"provider": name, "kind": e.kind})

    def quote_exact(self, chain: str, pool: str, token: str, budget: Budget) -> tuple[dict | None, str | None]:
        """Exact-pool quote: DEX Screener pair first, GeckoTerminal pool as fallback.
        Returns (snapshot, None) or (None, reason). Identity must match (pool and token)."""
        errs = []
        if chain == "solana" and budget.left(DEX) > 0:
            try:
                ps = self.dex.pairs(chain, [pool], budget, now())
                store.provider_ok(self.db, DEX)
                s = next((p for p in ps if p["pool_address"] == pool), None)
                if s and s["token_address"] == token:
                    return s, None
                errs.append("dex_pair_missing")
            except ProviderError as e:
                self._provider_fail(DEX, e, "FILLS")
                errs.append(f"dex_{e.kind}")
        if budget.left(GT) > 0:
            try:
                s = self.gt.pool(chain, pool, budget, now())
                store.provider_ok(self.db, GT)
                if s and s["token_address"] == token:
                    return s, None
                errs.append("gt_pool_missing")
            except ProviderError as e:
                self._provider_fail(GT, e, "FILLS")
                errs.append(f"gt_{e.kind}")
        return None, ("quote_unavailable:" + ",".join(errs)) if errs else "quote_budget_spent"

    def _eval(self, cycle_id, cand_or_snap, stage, outcome, reason, st_ver, data=None):
        s = cand_or_snap
        self.db.x("""INSERT INTO evaluations(cycle_id, at, chain, pool_address, token_address, candidate_id,
                     stage, outcome, reason, policy, settings_version, data_json) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)""",
                  (cycle_id, now(), s["chain"], s["pool_address"], s.get("token_address"),
                   stable_candidate_id(s["chain"], s["pool_address"]), stage, outcome, reason,
                   bench.POLICY.get(stage, stage), st_ver, jdump(data) if data is not None else None))

    def _reject(self, cycle_id, s, stage, reason, st, st_ver, snap_id=None, data=None, bench_it=True):
        self._eval(cycle_id, s, stage, "reject", reason, st_ver, data)
        if bench_it and reason != "too_young":
            bench.sit(self.db, s["chain"], s["pool_address"], stage, reason, st_ver)
        watchlist.record_check(self.db, s["chain"], s["pool_address"], stage, reason, st,
                               s.get("pool_created_at"), snap_id)

    # ------------------------------------------------------------------ cycle
    def run_cycle(self, lease: Lease, kind: str = "scheduled") -> dict:
        db = self.db
        st_ver, st = store.active_settings(db)
        book.ensure_account(db, st["starting_cash_usd"])
        self.http.limiter.set_limit(GT, st["gt_per_minute"])
        judge = self.judge_for(st)
        jstat = judge.status()
        t0 = now()
        cycle_id = db.x("INSERT INTO cycles(started_at, kind, settings_version, engine) VALUES(?,?,?,?)",
                        (t0, kind, st_ver, judge.engine)).lastrowid
        stats = {"seen": 0, "new": 0, "due": 0, "pre": {}, "vet": {}, "valuation": {},
                 "judge": {}, "deferred": 0, "dossiers": 0, "survivors": 0, "requests": {}, "outages": []}

        def finish(outcome, reason=None, extra=None):
            stats["requests"] = dict(budget.spent) if budget else {}
            db.x("UPDATE cycles SET ended_at=?, outcome=?, reason=?, stats_json=? WHERE id=?",
                 (now(), outcome, reason, jdump({**stats, **(extra or {})}), cycle_id))
            book.record_equity(db)
            msg = f"cycle {cycle_id}: {outcome}" + (f" ({reason})" if reason else "")
            emit(db, "CHIEF", "cycle_end", msg, data={"cycle_id": cycle_id, "outcome": outcome,
                                                       "reason": reason, "stats": stats})
            if outcome != "held":
                self.notifier.send(f"PandaBrain {'[DEMO] ' if self.demo else ''}{msg}")
            return {"cycle_id": cycle_id, "outcome": outcome, "reason": reason, "stats": stats, **(extra or {})}

        budget = None
        held = book.position(db)
        if held:
            set_seat(db, "SCAN", "blocked", f"holding {held['symbol'] or held['pool_address'][:8]}: no scan")
            set_seat(db, "CHIEF", "holding", "position open; RISK owns the desk")
            return finish("held", "position_open")

        set_seat(db, "CHIEF", "running", f"cycle {cycle_id} started ({judge.engine})")
        emit(db, "CHIEF", "cycle_start", f"cycle {cycle_id} started; engine {judge.engine}; settings v{st_ver}",
             data={"cycle_id": cycle_id, "settings_version": st_ver})
        budget = Budget({GT: st["gt_calls_per_cycle"], DEX: st["dex_calls_per_cycle"],
                         RPC: st["rpc_calls_per_cycle"]})
        reserve = Budget({GT: 1, DEX: 1})       # held back for the pre-fill re-quote

        # ---------------- SCAN: discovery -------------------------------------------------------
        set_seat(db, "SCAN", "running", "listing fresh pools")
        fresh: dict[tuple, tuple[dict, int]] = {}
        for chain in st["chains"]:
            for page in range(1, st["new_pool_pages"] + 1):
                if budget.left(GT) <= 1 + min(st["max_dossiers_per_cycle"], 3):
                    break
                lease.renew()
                try:
                    pools = self.gt.new_pools(chain, page, budget, now())
                    store.provider_ok(db, GT)
                except ProviderError as e:
                    self._provider_fail(GT, e, "SCAN")
                    stats["outages"].append(f"{GT}:{e.kind}")
                    break
                for s in pools:
                    stats["seen"] += 1
                    sid = watchlist.save_snapshot(db, s)
                    if watchlist.upsert_discovered(db, s, sid):
                        stats["new"] += 1
                    fresh[(s["chain"], s["pool_address"])] = (s, sid)
        capped = watchlist.enforce_cap(db, st["watchlist_max"])
        wl = watchlist.counts(db)
        set_seat(db, "WATCH", "idle", f"{wl.get('active', 0)} active identities, {stats['new']} new",
                 count=wl.get("active", 0), detail=wl)
        if capped:
            emit(db, "WATCH", "watchlist_cap", f"retired {capped} oldest entries over the cap")

        # ---------------- SCAN: due watchlist entries, exact-pool re-quotes --------------------
        due = watchlist.due(db, st["recheck_batch"], st_ver)
        stats["due"] = len(due)
        need: dict[str, list[dict]] = {}
        snaps: dict[tuple, tuple[dict, int]] = {}
        for w in due:
            k = (w["chain"], w["pool_address"])
            if k in fresh:
                snaps[k] = fresh[k]
            else:
                need.setdefault(w["chain"], []).append(w)
        quote_fail: dict[tuple, str] = {}
        for chain, ws in need.items():
            for i in range(0, len(ws), 30):
                chunk = ws[i:i + 30]
                if budget.left(GT) <= 1 + min(st["max_dossiers_per_cycle"], 3):
                    for w in chunk:
                        quote_fail[(chain, w["pool_address"])] = "deferred_budget"
                    continue
                lease.renew()
                try:
                    got = self.gt.pools_multi(chain, [w["pool_address"] for w in chunk], budget, now())
                    store.provider_ok(db, GT)
                except ProviderError as e:
                    self._provider_fail(GT, e, "SCAN")
                    stats["outages"].append(f"{GT}:{e.kind}")
                    for w in chunk:
                        quote_fail[(chain, w["pool_address"])] = "quote_unavailable"
                    continue
                by_pool = {g["pool_address"]: g for g in got}
                for w in chunk:
                    g = by_pool.get(w["pool_address"])
                    if g and g["token_address"] == w["token_address"]:
                        snaps[(chain, w["pool_address"])] = (g, watchlist.save_snapshot(db, g))
                    else:
                        quote_fail[(chain, w["pool_address"])] = "pool_not_found" if not g else "pool_identity_mismatch"

        tnow = now()
        pre_pass: list[tuple[dict, int]] = []
        for w in due:
            k = (w["chain"], w["pool_address"])
            if k in quote_fail:
                r = quote_fail[k]
                if r in ("deferred_budget", "quote_unavailable"):
                    stats["deferred"] += 1
                    db.x("UPDATE watchlist SET next_check_at=?, last_stage=? WHERE chain=? AND pool_address=?",
                         (tnow + 300, "deferred:" + r, w["chain"], w["pool_address"]))
                else:
                    self._reject(cycle_id, w, "pre", r, st, st_ver)
                    stats["pre"][r] = stats["pre"].get(r, 0) + 1
                continue
            s, sid = snaps[k]
            r = filters.pre_dossier(s, st, tnow)
            if r:
                self._reject(cycle_id, s, "pre", r, st, st_ver, sid)
                stats["pre"][r] = stats["pre"].get(r, 0) + 1
            else:
                pre_pass.append((s, sid))
        # turnover only orders the queue; it decides nothing
        pre_pass.sort(key=lambda x: -((x[0].get("volume_h24") or 0) / max(x[0].get("liquidity_usd") or 1, 1)))
        set_seat(db, "SCAN", "idle", f"{stats['seen']} seen, {stats['due']} due, {len(pre_pass)} passed arithmetic",
                 count=len(pre_pass), metric=stats["seen"],
                 detail={"pre": stats["pre"], "budget_gt": f"{budget.spent.get(GT, 0)}/{budget.limits[GT]}"})
        emit(db, "SCAN", "scan_done", f"{stats['seen']} seen, {stats['new']} new, {stats['due']} due, "
             f"{len(pre_pass)} passed pre-dossier", data={"pre_rejections": stats["pre"],
                                                          "outages": stats["outages"]})

        # ---------------- VET ----------------------------------------------------------------
        vetted: list[dict] = []
        slots = st["max_dossiers_per_cycle"]
        for s, sid in pre_pass:
            if slots <= 0:
                stats["deferred"] += 1
                db.x("UPDATE watchlist SET next_check_at=?, last_stage='deferred:dossier_slots', last_reason=NULL "
                     "WHERE chain=? AND pool_address=?", (now() + 120, s["chain"], s["pool_address"]))
                continue
            slots -= 1
            lease.renew()
            set_seat(db, "VET", "running", f"verifying {s.get('symbol') or s['token_address'][:8]}")
            cand, reason, stage = self._vet(cycle_id, s, st, budget)
            stats["dossiers"] += 1
            if reason:
                self._reject(cycle_id, s, stage, reason, st, st_ver, sid, data=cand,
                             bench_it=True)
                stats[stage][reason] = stats[stage].get(reason, 0) + 1
                emit(db, "VET", "vet_reject", f"{s.get('symbol') or '?'} rejected at {stage}: {reason}",
                     data={"candidate_id": cand["candidate_id"], "reason": reason, "stage": stage})
                continue
            vetted.append(cand)
        set_seat(db, "VET", "idle", f"{stats['dossiers']} dossiers, {len(vetted)} verified",
                 count=len(vetted), metric=stats["dossiers"])

        # ---------------- SOCIAL -------------------------------------------------------------
        for c in vetted:
            c["social"] = social_evidence(c.get("twitter_handle_raw"), self.social)
            if c.get("dossier_id"):
                self.db.x("UPDATE dossiers SET data_json=? WHERE id=?",
                          (jdump({k: v for k, v in c.items() if k != "snapshot"} | {"vet_result": None}), c["dossier_id"]))
        soc_state = "idle" if self.social.connected else "unconfigured"
        set_seat(db, "SOCIAL", soc_state,
                 f"{len(vetted)} handles checked; adapter {'connected' if self.social.connected else 'disconnected'}",
                 count=sum(1 for c in vetted if c["social"]["status"] == "read"),
                 detail={c["candidate_id"]: c["social"]["status"] for c in vetted})

        if not vetted:
            set_seat(db, "CHIEF", "idle", "no candidate survived the facts")
            return finish("no_trade", "no_verified_candidates" if not stats["outages"] else "provider_outage")

        # ---------------- JUDGE + PICK (CHIEF) -----------------------------------------------
        if not jstat["configured"]:
            set_seat(db, "CHIEF", "unconfigured", jstat["detail"])
            for c in vetted:
                watchlist.record_check(db, c["chain"], c["pool_address"], "judge", "judge_unconfigured", st,
                                       c["snapshot"].get("pool_created_at"))
            return finish("no_trade", "judge_unconfigured", {"judge": jstat})
        set_seat(db, "CHIEF", "running", f"judging {len(vetted)} with {judge.engine}")
        survivors = []
        try:
            acct = book.account(db)
            for c in vetted:
                lease.renew()
                c["cash_usd"] = acct["cash_usd"]
                j = judge.judge(c, st)
                self._store_judgment(cycle_id, c["candidate_id"], j.engine, j.model_id, "candidate",
                                     j.score_type, st_ver, j.raw, j.as_dict())
                if j.passed:
                    survivors.append((c, j))
                    self._eval(cycle_id, c, "judge", "pass", None, st_ver, {"summary": j.summary})
                    watchlist.record_check(db, c["chain"], c["pool_address"], "judge", None, st,
                                           c["snapshot"].get("pool_created_at"))
                else:
                    self._reject(cycle_id, c, "judge", j.reason, st, st_ver, data={"summary": j.summary})
                    stats["judge"][j.reason] = stats["judge"].get(j.reason, 0) + 1
            stats["survivors"] = len(survivors)
            lease.renew()
            p = judge.pick(survivors, st) if survivors else None
            if p is not None:
                self._store_judgment(cycle_id, p.candidate_id, p.engine, p.model_id, "pick", p.score_type,
                                     st_ver, [p.raw] if p.raw else [], p.as_dict())
        except JudgeUnavailable as e:
            set_seat(db, "CHIEF", "error", sanitize(e))
            emit(db, "CHIEF", "judge_unavailable", f"{judge.engine} unavailable: {e}. No fallback.", level="warn")
            return finish("no_trade", "judge_unavailable")
        except JudgeSchemaError as e:
            set_seat(db, "CHIEF", "error", sanitize(e))
            emit(db, "CHIEF", "judge_schema_error", f"{judge.engine} contract violation: {e}", level="error")
            return finish("no_trade", "judge_schema_error")

        if not survivors:
            set_seat(db, "CHIEF", "idle", "every verified candidate failed the judge gates")
            return finish("no_trade", "judge_rejected_all")
        if p.candidate_id is None:
            set_seat(db, "CHIEF", "idle", f"no pick: {p.reason}")
            emit(db, "CHIEF", "no_pick", f"no trade: {p.reason}", data=p.as_dict())
            return finish("no_trade", p.reason, {"pick": p.as_dict()})
        chosen = next((x for x in survivors if x[0]["candidate_id"] == p.candidate_id), None)
        if chosen is None:
            emit(db, "CHIEF", "pick_unknown", "pick named a candidate outside the shortlist", level="error")
            return finish("no_trade", "pick_unknown_candidate")
        c, j = chosen
        handoff = {"cycle_id": cycle_id, "candidate_id": c["candidate_id"], "chain": c["chain"],
                   "pool_address": c["pool_address"], "token_address": c["token_address"],
                   "symbol": c.get("symbol"), "engine": p.engine, "model_id": p.model_id,
                   "score_type": p.score_type, "confidence": p.confidence, "pick_detail": p.detail,
                   "size_factor": j.size_factor, "size_notes": j.size_notes, "judgment": j.values,
                   "valuation": c.get("valuation"), "social": c.get("social"), "settings_version": st_ver}
        set_seat(db, "CHIEF", "running", f"picked {c.get('symbol') or c['candidate_id']}; handing to SIZE",
                 detail={"candidate_id": c["candidate_id"]})
        emit(db, "CHIEF", "handoff", f"pick {c.get('symbol') or '?'} ({c['candidate_id']}) via {p.engine}",
             data=handoff)

        # ---------------- SIZE ---------------------------------------------------------------
        sz = sizing.size(acct["cash_usd"], c["snapshot"].get("liquidity_usd"), j.size_factor, st)
        handoff["sizing"] = sz
        if sz["reason"]:
            set_seat(db, "SIZE", "idle", f"ticket 0: {sz['reason']}")
            emit(db, "SIZE", "size_zero", f"ticket 0 ({sz['reason']})", data=sz)
            set_seat(db, "CHIEF", "idle", f"no trade: size {sz['reason']}")
            return finish("no_trade", f"size_{sz['reason']}", {"handoff": handoff})
        set_seat(db, "SIZE", "idle", f"ticket ${Decimal(sz['ticket_usd']):,.2f}", metric=float(sz["ticket_usd"]),
                 detail=sz)
        emit(db, "SIZE", "sized", f"ticket ${sz['ticket_usd']}", data=sz)

        # ---------------- FILLS (paper) ------------------------------------------------------
        set_seat(db, "FILLS", "running", "re-quoting exact pool before simulated fill")
        lease.renew()
        q, why = self.quote_exact(c["chain"], c["pool_address"], c["token_address"], reserve)
        if q is None or filters.pre_dossier(q, st, now()) == "stale_quote":
            reason = why or "stale_quote"
            set_seat(db, "FILLS", "blocked", f"no fill: {reason}")
            emit(db, "FILLS", "fill_refused", f"no fill: {reason}", level="warn")
            set_seat(db, "CHIEF", "idle", "no trade: fill quote unavailable")
            return finish("no_trade", "fill_quote_unavailable", {"handoff": handoff})
        watchlist.save_snapshot(db, q)
        try:
            fill = self.adapter.entry(q["price_usd"], q.get("liquidity_usd"), sz["ticket_usd"], st)
        except FillRefused as e:
            set_seat(db, "FILLS", "blocked", f"no fill: {e.reason}")
            emit(db, "FILLS", "fill_refused", f"no fill: {e.reason}", level="warn")
            set_seat(db, "CHIEF", "idle", f"no trade: {e.reason}")
            return finish("no_trade", e.reason, {"handoff": handoff})
        fill["quote_source"], fill["quote_fetched_at"] = q["source"], q["fetched_at"]
        handoff["fill"] = fill
        try:
            res = book.open_position(db, lease, f"open:{cycle_id}:{c['pool_address']}", c, fill, handoff)
        except book.PositionHeld:
            return finish("no_trade", "position_already_open")
        set_seat(db, "FILLS", "idle", f"paper fill {fill['slippage_bps']}bps, fee ${Decimal(fill['fee_usd']):.2f}",
                 detail=fill)
        emit(db, "FILLS", "paper_fill", f"SIMULATED buy {c.get('symbol') or '?'} ${fill['ticket_usd']} "
             f"@ {fill['fill_price']} ({fill['slippage_bps']}bps)", data={**fill, **res})
        set_seat(db, "RISK", "holding", "timer started at fill")
        set_seat(db, "SCAN", "blocked", "position open: no scan")
        set_seat(db, "CHIEF", "holding", f"holding {c.get('symbol') or c['candidate_id']}")
        return finish("opened", None, {"position_uid": res["position_uid"]})

    def _store_judgment(self, cycle_id, cand_id, engine, model_id, qset, score_type, st_ver, raw, result):
        from .config import THRESHOLD_VERSION
        self.db.x("""INSERT INTO judgments(cycle_id, at, candidate_id, engine, model_id, question_set, score_type,
                     threshold_version, settings_version, raw_json, result_json) VALUES(?,?,?,?,?,?,?,?,?,?,?)""",
                  (cycle_id, now(), cand_id, engine, model_id, qset, score_type, THRESHOLD_VERSION, st_ver,
                   jdump(raw), jdump(result)))

    def _vet(self, cycle_id, s, st, budget) -> tuple[dict, str | None, str]:
        cand, reason, stage = self._vet_inner(s, st, budget)
        if "holder_count" in cand:          # a dossier was actually fetched
            cand["dossier_id"] = self.db.x(
                "INSERT INTO dossiers(cycle_id, at, chain, pool_address, candidate_id, data_json) VALUES(?,?,?,?,?,?)",
                (cycle_id, now(), s["chain"], s["pool_address"], cand["candidate_id"],
                 jdump({k: v for k, v in cand.items() if k != "snapshot"} | {"vet_result": reason, "stage": stage}))).lastrowid
        return cand, reason, stage

    def _vet_inner(self, s, st, budget) -> tuple[dict, str | None, str]:
        cid = stable_candidate_id(s["chain"], s["pool_address"])
        t = now()
        cand = {"candidate_id": cid, "chain": s["chain"], "pool_address": s["pool_address"],
                "token_address": s["token_address"], "symbol": s.get("symbol"), "snapshot": s,
                "age_minutes": filters.age_minutes(s, t)}
        # token info (GeckoTerminal)
        try:
            ti = self.gt.token_info(s["chain"], s["token_address"], budget)
            store.provider_ok(self.db, GT)
        except ProviderError as e:
            self._provider_fail(GT, e, "VET")
            return cand, "token_info_unavailable", "vet"
        cand.update({"holder_count": ti["holder_count"], "top_10_percent": ti["top_10_percent"],
                     "twitter_handle_raw": ti["twitter_handle"], "gt_score": ti["gt_score"],
                     "token_info_fetched_at": t})
        # mint verification (RPC, parsed + raw)
        try:
            parsed = self.rpc.account_parsed(s["token_address"], budget)
            raw = self.rpc.account_raw(s["token_address"], budget)
            store.provider_ok(self.db, RPC, self.rpc.display)
        except ProviderError as e:
            self._provider_fail(RPC, e, "VET")
            return cand, "rpc_unavailable", "vet"
        mc = verify_mint(s["token_address"], parsed, raw, st["allowed_token2022_extensions"])
        cand["mint"] = mc.as_dict()
        if not mc.ok:
            return cand, mc.reason, "vet"
        # concentration (largest token account; may be the pool vault)
        try:
            la = self.rpc.largest_accounts(s["token_address"], budget)
            vals = (la or {}).get("value") or []
            top = int(vals[0]["amount"]) if vals and str(vals[0].get("amount", "")).isdigit() else None
            cand["largest_account_share"] = (float(Decimal(top) / Decimal(mc.raw_supply))
                                             if top is not None else None)
        except ProviderError as e:
            self._provider_fail(RPC, e, "VET")
            cand["largest_account_share"] = None
        cand["dossier_at"] = now()
        r = filters.vet_facts(cand, st)
        if r:
            return cand, r, "vet"
        v = valuation.choose(s, mc, st)
        cand["valuation"] = v["valuation"]
        if not v["ok"]:
            return cand, v["reason"], "valuation"
        return cand, None, "vet"

    # ------------------------------------------------------------------ RISK
    def risk_tick(self, lease: Lease, force: bool = False) -> dict | None:
        db = self.db
        p = book.position(db)
        if not p:
            set_seat(db, "RISK", "idle", "no position")
            return None
        st_ver, st = store.active_settings(db)
        t = now()
        # pending closes retry sooner, but never faster than every 30 s
        interval = st["risk_poll_seconds"] if p["status"] == "OPEN" else min(st["risk_poll_seconds"], 30)
        if not force and p["last_risk_at"] and t - p["last_risk_at"] < interval:
            return {"skipped": True}
        db.x("UPDATE position SET last_risk_at=? WHERE position_uid=?", (t, p["position_uid"]))
        lease.renew()
        set_seat(db, "RISK", "running", "marking exact pool")
        q, why = self.quote_exact(p["chain"], p["pool_address"], p["token_address"], Budget({GT: 1, DEX: 1}))
        if q is not None and (t - (q.get("fetched_at") or 0)) > st["max_quote_age_seconds"]:
            q, why = None, "stale_quote"
        if q is None:
            fails = p["mark_fail_count"] + 1
            db.x("UPDATE position SET mark_fail_count=? WHERE position_uid=?", (fails, p["position_uid"]))
            if fails >= st["quote_fail_close_after"] and p["status"] == "OPEN":
                book.request_close(db, p["position_uid"], "unmeasurable")
                emit(db, "RISK", "close_requested", f"cannot measure after {fails} tries; close requested "
                     "(pending until a real exit price exists)", level="warn")
            set_seat(db, "RISK", "blocked" if p["status"] == "CLOSE_PENDING" or fails >= st["quote_fail_close_after"]
                     else "holding", f"mark failed ({why}); {fails} in a row")
            return {"marked": False, "reason": why}
        watchlist.save_snapshot(db, q)
        book.record_mark(db, p["position_uid"], q)
        price = dec(q["price_usd"])
        entry_q = dec(p["quote_price"])
        reason = None
        if p["status"] == "CLOSE_PENDING":
            reason = p["close_reason"] or "close_pending"
        elif price <= entry_q * (1 - Decimal(str(st["stop_loss_pct"]))):
            reason = "stop_loss"
        elif price >= entry_q * (1 + Decimal(str(st["take_profit_pct"]))):
            reason = "take_profit"
        elif (t - p["opened_at"]) / 60 >= st["max_hold_minutes"]:
            reason = "max_hold"
        else:
            v6, v24 = fnum(q.get("volume_h6")), fnum(q.get("volume_h24"))
            if v6 is not None and v24:
                ratio = v6 / (v24 / 4)
                if ratio < st["volume_ratio_exit"]:
                    reason = "volume_fade"
        book.record_equity(db)
        if not reason:
            mins = (t - p["opened_at"]) / 60
            set_seat(db, "RISK", "holding", f"held {mins:.0f}m; mark {q['price_usd']}",
                     metric=float(price / entry_q - 1))
            return {"marked": True, "close": False}
        book.request_close(db, p["position_uid"], reason)
        try:
            ex = self.adapter.exit(q["price_usd"], q.get("liquidity_usd"), p["qty"], st)
        except FillRefused as e:
            set_seat(db, "RISK", "blocked", f"close pending: {e.reason}")
            return {"marked": True, "close": "pending"}
        ex["quote_source"], ex["quote_fetched_at"] = q["source"], q["fetched_at"]
        res = book.close_position(db, lease, p["position_uid"], ex, reason)
        bench.sit(db, p["chain"], p["pool_address"], "book", "recently_closed", st_ver)
        book.record_equity(db)
        emit(db, "RISK", "paper_close", f"SIMULATED close {p['symbol'] or '?'}: {reason}, "
             f"pnl ${Decimal(res.get('pnl_usd', '0')):.2f}", data={**ex, **res, "reason": reason})
        set_seat(db, "RISK", "idle", f"closed: {reason}")
        set_seat(db, "SCAN", "idle", "book released; scanning resumes next cycle")
        set_seat(db, "CHIEF", "idle", f"closed {p['symbol'] or ''} ({reason})")
        self.notifier.send(f"PandaBrain {'[DEMO] ' if self.demo else ''}paper close {p['symbol']}: {reason}")
        return {"marked": True, "close": True, "reason": reason, **res}


def run_once_cli(desk: Desk, owner: str | None = None) -> dict:
    """One cycle (or one RISK tick when holding) under the engine lease."""
    lease = Lease(desk.db, owner=owner)
    if not lease.acquire():
        return {"error": "another worker holds the engine lease"}
    try:
        if book.position(desk.db):
            return {"risk": desk.risk_tick(lease, force=True)}
        return desk.run_cycle(lease, kind="manual")
    except LeaseLost as e:
        return {"error": str(e)}
    finally:
        lease.release()


def wait(seconds: float):  # indirection for tests
    time.sleep(seconds)
