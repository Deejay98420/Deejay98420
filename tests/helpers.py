import os
import shutil
import tempfile
import unittest

os.environ["PANDABRAIN_NO_OUTBOUND"] = "1"          # no messages are ever sent during tests
os.environ.pop("TYPESAFE_API_KEY", None)

from pandabrain import store  # noqa: E402
from pandabrain.db import open_book  # noqa: E402
from pandabrain.engine import Desk  # noqa: E402
from pandabrain.providers import Http  # noqa: E402
from pandabrain.providers.fixtures import FixtureWorld, default_specs  # noqa: E402

FAST = {"max_quote_age_seconds": 600, "risk_poll_seconds": 10}


class DeskCase(unittest.TestCase):
    mode = "demo"

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="pbtest-")
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def make(self, specs=None, settings=None, mode=None, world=None, sub="book", **kw):
        db = open_book(os.path.join(self.tmp, sub), mode or self.mode)
        self.addCleanup(db.close)
        w = world or FixtureWorld(specs=specs)
        if specs is None and world is None:
            w.specs = default_specs(w.t0)
        desk = Desk(db, Http(transport=w, sleep=lambda s: None), demo=(mode or self.mode) == "demo", **kw)
        patch = dict(FAST)
        patch.update(settings or {})
        store.save_settings(db, patch, "test", None)
        return desk, w, db


def only(world, *syms):
    world.specs = [s for s in world.specs if s["sym"] in syms]
    return world
