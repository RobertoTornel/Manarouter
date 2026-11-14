"""CPU-only tests for the Modal fine-tuning reference provider.

The deployed app needs Modal, FastAPI and a GPU. None of those are needed here: this file
installs small stand-ins for the Modal/FastAPI/uvicorn modules that ``app.py`` imports at
module scope, loads ``app.py`` from its path, and exercises the store and the pure helpers
against a temporary directory. Run it with::

    python3 test_app.py

or with pytest, which collects the same ``unittest`` cases. Nothing here starts uvicorn,
imports torch, or touches the network.
"""

from __future__ import annotations

import asyncio
import importlib.util
import json
import sys
import tempfile
import time
import types
import unittest
from pathlib import Path

APP_PATH = Path(__file__).with_name("app.py")


# --------------------------------------------------------------------------------------
# Minimal stand-ins for the modules app.py imports at module scope
# --------------------------------------------------------------------------------------


class _FakeImage:
    def __init__(self) -> None:
        self.calls: list[tuple] = []

    @classmethod
    def debian_slim(cls, **kwargs):
        return cls()

    def apt_install(self, *packages, **kwargs):
        self.calls.append(("apt_install", packages, kwargs))
        return self

    def run_commands(self, *commands, **kwargs):
        self.calls.append(("run_commands", commands, kwargs))
        return self

    def pip_install(self, *packages, **kwargs):
        self.calls.append(("pip_install", packages, kwargs))
        return self

    def env(self, values=None, **kwargs):
        self.calls.append(("env", values, kwargs))
        return self


class _FakeVolume:
    def __init__(self, name: str) -> None:
        self.name = name
        self.commits = 0
        self.reloads = 0

    @classmethod
    def from_name(cls, name, create_if_missing=False):
        instance = cls(name)
        instance.create_if_missing = create_if_missing
        return instance

    def commit(self):
        self.commits += 1

    def reload(self):
        self.reloads += 1


class _FakeApp:
    def __init__(self, name: str) -> None:
        self.name = name
        self.classes: list[type] = []
        self.cls_kwargs: dict = {}

    def cls(self, **kwargs):
        self.cls_kwargs = kwargs

        def decorator(obj):
            self.classes.append(obj)
            return obj

        return decorator

    def function(self, **kwargs):
        return lambda obj: obj


def _identity_decorator(**kwargs):
    def decorator(obj):
        obj.__decorator_kwargs__ = kwargs
        return obj

    return decorator


def _build_fake_modal() -> types.ModuleType:
    module = types.ModuleType("modal")
    module.App = _FakeApp
    module.Image = _FakeImage
    module.Volume = _FakeVolume
    module.Secret = type("Secret", (), {"from_name": classmethod(lambda cls, name: cls())})
    module.concurrent = _identity_decorator
    module.web_server = _identity_decorator
    module.method = _identity_decorator
    module.enter = _identity_decorator
    module.exit = _identity_decorator
    return module


class _FakeHTTPException(Exception):
    def __init__(self, status_code: int, detail=None, **kwargs) -> None:
        super().__init__(detail)
        self.status_code = status_code
        self.detail = detail


class _FakeRequest:
    pass


class _FakeUploadFile:
    filename = "upload.jsonl"

    async def read(self):
        return b""


class _FakeFileDefault:
    pass


class _FakeFormDefault:
    def __init__(self, default):
        self.default = default


class _FakeResponse:
    def __init__(self, content=b"", media_type=None, headers=None, status_code=200, **kwargs):
        self.content = content
        self.media_type = media_type
        self.headers = headers or {}
        self.status_code = status_code


class _FakeJSONResponse(_FakeResponse):
    def __init__(self, content=None, status_code=200, **kwargs):
        super().__init__(content=content, status_code=status_code, **kwargs)


class _FakeFastAPI:
    def __init__(self, **kwargs) -> None:
        self.title = kwargs.get("title")
        self.routes: list[tuple[str, str, object]] = []
        self.exception_handlers: dict = {}

    def _route(self, method: str):
        def decorator(path: str):
            def register(func):
                self.routes.append((method, path, func))
                return func

            return register

        return decorator

    def get(self, path, **kwargs):
        return self._route("GET")(path)

    def post(self, path, **kwargs):
        return self._route("POST")(path)

    def delete(self, path, **kwargs):
        return self._route("DELETE")(path)

    def exception_handler(self, exc_type):
        def register(func):
            self.exception_handlers[exc_type] = func
            return func

        return register


def _build_fake_fastapi() -> tuple[types.ModuleType, types.ModuleType]:
    module = types.ModuleType("fastapi")
    module.FastAPI = _FakeFastAPI
    module.File = lambda *args, **kwargs: _FakeFileDefault()
    module.Form = lambda default=None, **kwargs: _FakeFormDefault(default)
    module.HTTPException = _FakeHTTPException
    module.Request = _FakeRequest
    module.UploadFile = _FakeUploadFile

    responses = types.ModuleType("fastapi.responses")
    responses.Response = _FakeResponse
    responses.JSONResponse = _FakeJSONResponse
    module.responses = responses
    return module, responses


def _build_fake_uvicorn() -> types.ModuleType:
    module = types.ModuleType("uvicorn")

    class Config:
        def __init__(self, *args, **kwargs):
            self.args = args
            self.kwargs = kwargs

    class Server:
        def __init__(self, config):
            self.config = config
            self.started = False
            self.should_exit = False

        def run(self):
            self.started = True

    module.Config = Config
    module.Server = Server
    return module


def _load_app() -> types.ModuleType:
    modal = _build_fake_modal()
    fastapi, responses = _build_fake_fastapi()
    uvicorn = _build_fake_uvicorn()
    # Keep references alive for the lifetime of the process: the imported module holds
    # attributes of these fake modules, and some code imports uvicorn lazily.
    sys.modules["modal"] = modal
    sys.modules["fastapi"] = fastapi
    sys.modules["fastapi.responses"] = responses
    sys.modules["uvicorn"] = uvicorn

    spec = importlib.util.spec_from_file_location("finetune_reference_app", APP_PATH)
    if spec is None or spec.loader is None:  # pragma: no cover - only if the file is gone
        raise RuntimeError(f"could not load {APP_PATH}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


appmod = _load_app()


# --------------------------------------------------------------------------------------
# Test helpers
# --------------------------------------------------------------------------------------

GOOD_EXAMPLE = {
    "messages": [
        {"role": "user", "content": "What is 2+2?"},
        {"role": "assistant", "content": "4"},
    ]
}
GOOD_LINE = json.dumps(GOOD_EXAMPLE) + "\n"


class StoreTestCase(unittest.TestCase):
    """Give every test an isolated store, so no state leaks between cases."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        appmod.STORE = appmod.Store(self.tmp.name)
        appmod._ACTIVE_JOB = None
        with appmod._MODEL_LOCK:
            appmod._MODEL_CACHE.clear()

    def store(self):
        return appmod.STORE

    def upload(self, text: str = GOOD_LINE, filename: str = "train.jsonl") -> dict:
        return self.store().create_file(filename, "fine-tune", text.encode("utf-8"))

    def make_job(self, **overrides) -> dict:
        payload = {
            "training_file": self.upload()["id"],
            "model": "Qwen/Qwen2.5-1.5B-Instruct",
        }
        payload.update(overrides)
        return self.store().create_job(payload)

    def make_running_job(self, **overrides) -> dict:
        job = self.make_job(**overrides)
        self.store().set_status(job["id"], "queued")
        self.store().set_status(job["id"], "running")
        return self.store().get_job(job["id"])

    def assertApiError(self, callable_, status_code: int, code: str | None = None):
        with self.assertRaises(appmod.ApiError) as caught:
            callable_()
        error = caught.exception
        self.assertEqual(error.status_code, status_code, error.message)
        if code is not None:
            self.assertEqual(error.code, code, error.message)
        return error


# --------------------------------------------------------------------------------------
# Jobs: creation, transitions, events
# --------------------------------------------------------------------------------------


class JobStoreTests(StoreTestCase):
    def test_new_job_is_validating_with_an_active_marker(self) -> None:
        job = self.make_job()
        self.assertEqual(job["object"], "fine_tuning.job")
        self.assertEqual(job["status"], "validating_files")
        self.assertIsNone(job["fine_tuned_model"])
        self.assertIsNone(job["finished_at"])
        self.assertEqual(job["trained_tokens"], None)
        self.assertEqual(job["hyperparameters"], {
            "n_epochs": 1,
            "batch_size": 1,
            "learning_rate_multiplier": 1.0,
        })
        self.assertTrue((self.store().jobs / "ACTIVE.json").exists())
        self.assertFalse(any(key.startswith("_") for key in appmod.public_job(job)))

        events = self.store().list_events(job["id"])
        self.assertGreaterEqual(len(events), 1)
        self.assertEqual(events[0]["object"], "fine_tuning.job.event")
        self.assertTrue(events[0]["id"].startswith("ftevent-"))
        self.assertEqual(events[0]["type"], "message")
        self.assertIn("Created job", events[0]["message"])

    def test_status_transitions_and_terminal_event(self) -> None:
        job = self.make_job()
        self.store().set_status(job["id"], "queued")
        self.assertEqual(self.store().get_job(job["id"])["status"], "queued")
        self.store().set_status(job["id"], "running")
        self.assertEqual(self.store().get_job(job["id"])["status"], "running")

        self.store().append_event(
            job["id"],
            "metrics",
            "Step 1: train_loss=0.5000",
            {"step": 1, "train_loss": 0.5, "learning_rate": 0.0002},
        )
        finished = self.store().finish_job(job["id"], "succeeded", trained_tokens=42)
        self.assertEqual(finished["status"], "succeeded")
        self.assertEqual(finished["trained_tokens"], 42)
        self.assertIsNotNone(finished["finished_at"])
        self.assertEqual(finished["fine_tuned_model"], finished["_fine_tuned_model"])
        self.assertFalse((self.store().jobs / "ACTIVE.json").exists())

        events = self.store().list_events(job["id"])
        metrics = [event for event in events if event["type"] == "metrics"]
        self.assertEqual(len(metrics), 1)
        self.assertEqual(metrics[0]["level"], "info")
        self.assertEqual(metrics[0]["data"], {
            "step": 1,
            "train_loss": 0.5,
            "learning_rate": 0.0002,
        })
        self.assertEqual(events[-1]["level"], "info")
        self.assertIn("succeeded", events[-1]["message"])

    def test_failed_job_records_error_and_error_event(self) -> None:
        job = self.make_job()
        self.store().finish_job(
            job["id"],
            "failed",
            error={"code": "invalid_training_file", "message": "bad line", "param": "training_file"},
        )
        finished = self.store().get_job(job["id"])
        self.assertEqual(finished["status"], "failed")
        self.assertEqual(finished["error"]["code"], "invalid_training_file")
        self.assertIsNone(finished["fine_tuned_model"])
        event = self.store().list_events(job["id"])[-1]
        self.assertEqual(event["level"], "error")
        self.assertEqual(event["data"]["status"], "failed")
        self.assertEqual(event["data"]["error"]["code"], "invalid_training_file")

    def test_finish_is_idempotent_for_a_terminal_job(self) -> None:
        job = self.make_job()
        self.store().finish_job(job["id"], "cancelled")
        again = self.store().finish_job(job["id"], "succeeded")
        self.assertEqual(again["status"], "cancelled")
        self.assertIsNone(again["fine_tuned_model"])

    def test_jobs_are_listed_newest_first_even_in_the_same_second(self) -> None:
        first = self.make_job()
        self.store().finish_job(first["id"], "cancelled")
        second = self.make_job()
        listed = [job["id"] for job in self.store().list_jobs()]
        self.assertEqual(listed[0], second["id"])
        self.assertIn(first["id"], listed)

    def test_second_job_is_refused_and_leaves_no_partial_job(self) -> None:
        first = self.make_job()
        error = self.assertApiError(
            lambda: self.make_job(), 409, code="job_running"
        )
        self.assertIn(first["id"], error.message)
        self.assertEqual(len(self.store().list_jobs()), 1)
        self.assertEqual(len(self.store().list_events(first["id"])), 1)

        # A terminal job releases the GPU marker, so a new job is accepted afterwards.
        self.store().finish_job(first["id"], "cancelled")
        second = self.make_job()
        self.assertEqual(second["status"], "validating_files")

    def test_stale_active_job_is_failed_and_replaced(self) -> None:
        first = self.make_job()
        path = self.store().job_dir(first["id"]) / "job.json"
        stale = json.loads(path.read_text(encoding="utf-8"))
        stale["_updated_at"] = int(time.time()) - appmod.ACTIVE_STALE_SECONDS - 60
        self.store()._write_json(path, stale)

        second = self.make_job()
        self.assertEqual(second["status"], "validating_files")
        old = self.store().get_job(first["id"])
        self.assertEqual(old["status"], "failed")
        self.assertEqual(old["error"]["code"], "container_stopped")

    def test_pagination_walks_events_without_duplicates(self) -> None:
        job = self.make_job()
        for step in range(1, 7):
            self.store().append_event(
                job["id"], "info", f"step {step}", {"step": step}, event_type="metrics"
            )
        events = self.store().list_events(job["id"])
        self.assertEqual(len(events), 7)

        seen: list[str] = []
        cursor = None
        pages = 0
        while True:
            page, has_more = appmod._page(events, 2, cursor)
            self.assertLessEqual(len(page), 2)
            seen.extend(event["id"] for event in page)
            pages += 1
            if not has_more:
                break
            cursor = page[-1]["id"]
        self.assertEqual(pages, 4)
        self.assertEqual(seen, [event["id"] for event in events])
        self.assertTrue(appmod._page(events, 2, events[-1]["id"]) == ([], False))

    def test_unknown_after_cursor_starts_at_the_beginning(self) -> None:
        job = self.make_job()
        page, has_more = appmod._page(self.store().list_events(job["id"]), 1, "ftevent-nope")
        self.assertGreaterEqual(len(page), 1)
        self.assertTrue(has_more or len(page) == 1)
        self.assertEqual(appmod._after_index([{"id": "a"}, {"id": "b"}], "b"), 2)
        self.assertEqual(appmod._after_index([{"id": "a"}], None), 0)

    def test_limits_are_clamped(self) -> None:
        self.assertEqual(appmod._clamp_limit(None), 20)
        self.assertEqual(appmod._clamp_limit("nonsense"), 20)
        self.assertEqual(appmod._clamp_limit(0), 1)
        self.assertEqual(appmod._clamp_limit(-4), 1)
        self.assertEqual(appmod._clamp_limit(500), 100)
        self.assertEqual(appmod._clamp_limit("3"), 3)

    def test_events_route_reports_has_more(self) -> None:
        job = self.make_job()
        for step in range(1, 5):
            self.store().append_event(
                job["id"], "info", f"step {step}", {"step": step}, event_type="metrics"
            )
        first = appmod.list_fine_tuning_events(job["id"], limit=2)
        self.assertEqual(first["object"], "list")
        self.assertEqual(len(first["data"]), 2)
        self.assertTrue(first["has_more"])
        second = appmod.list_fine_tuning_events(job["id"], limit=2, after=first["data"][-1]["id"])
        self.assertEqual(len(second["data"]), 2)
        self.assertTrue(second["has_more"])
        third = appmod.list_fine_tuning_events(job["id"], limit=2, after=second["data"][-1]["id"])
        self.assertEqual(len(third["data"]), 1)
        self.assertFalse(third["has_more"])


# --------------------------------------------------------------------------------------
# Pause / resume / cancel state machine
# --------------------------------------------------------------------------------------


class ControlStateTests(StoreTestCase):
    def test_job_is_not_pausable_before_it_runs(self) -> None:
        job = self.make_job()
        self.assertApiError(lambda: self.store().pause_job(job["id"]), 400, "job_not_pausable")

    def test_pause_resume_round_trip(self) -> None:
        job = self.make_running_job()
        paused = self.store().pause_job(job["id"])
        self.assertEqual(paused["status"], "running")
        self.assertTrue(self.store().read_control(job["id"])["pause"])
        self.assertApiError(lambda: self.store().pause_job(job["id"]), 400, "job_already_paused")

        resumed = self.store().resume_job(job["id"])
        self.assertEqual(resumed["status"], "running")
        self.assertFalse(self.store().read_control(job["id"])["pause"])
        self.assertApiError(lambda: self.store().resume_job(job["id"]), 400, "job_not_paused")

    def test_resume_requires_a_pause(self) -> None:
        job = self.make_running_job()
        self.assertApiError(lambda: self.store().resume_job(job["id"]), 400, "job_not_paused")

    def test_pause_and_resume_rejected_while_cancel_is_pending(self) -> None:
        job = self.make_running_job()
        self.store().cancel_job(job["id"])
        self.assertTrue(self.store().read_control(job["id"])["cancel"])
        self.assertApiError(lambda: self.store().pause_job(job["id"]), 400, "job_not_pausable")
        self.assertApiError(lambda: self.store().resume_job(job["id"]), 400, "job_not_resumable")

    def test_cancel_queued_job_is_immediate(self) -> None:
        job = self.make_job()
        self.store().set_status(job["id"], "queued")
        cancelled = self.store().cancel_job(job["id"])
        self.assertEqual(cancelled["status"], "cancelled")
        self.assertFalse((self.store().jobs / "ACTIVE.json").exists())

    def test_cancel_running_job_is_cooperative(self) -> None:
        job = self.make_running_job()
        requested = self.store().cancel_job(job["id"])
        self.assertEqual(requested["status"], "running")
        self.assertTrue(self.store().read_control(job["id"])["cancel"])
        self.assertIn("Cancellation requested", self.store().list_events(job["id"])[-1]["message"])
        self.store().finish_job(job["id"], "cancelled")

    def test_cancel_terminal_job_is_400_and_names_the_status(self) -> None:
        job = self.make_job()
        self.store().finish_job(job["id"], "succeeded")
        error = self.assertApiError(
            lambda: self.store().cancel_job(job["id"]), 400, "job_not_cancellable"
        )
        self.assertIn("succeeded", error.message)
        self.store().finish_job(job["id"], "cancelled")  # no-op; still succeeded
        self.assertEqual(self.store().get_job(job["id"])["status"], "succeeded")

    def test_pause_and_resume_after_finish_are_400(self) -> None:
        job = self.make_job()
        self.store().finish_job(job["id"], "cancelled")
        self.assertApiError(lambda: self.store().pause_job(job["id"]), 400, "job_not_pausable")
        self.assertApiError(lambda: self.store().resume_job(job["id"]), 400, "job_not_resumable")

    def test_missing_job_is_404(self) -> None:
        for operation in (
            self.store().cancel_job,
            self.store().pause_job,
            self.store().resume_job,
        ):
            self.assertApiError(lambda op=operation: op("ftjob-does-not-exist"), 404, "job_not_found")


# --------------------------------------------------------------------------------------
# Hyperparameters and the fine-tuned model name
# --------------------------------------------------------------------------------------


class HyperparameterTests(unittest.TestCase):
    def test_defaults(self) -> None:
        self.assertEqual(appmod.resolve_hyperparameters({}), {
            "n_epochs": 1,
            "batch_size": 1,
            "learning_rate_multiplier": 1.0,
        })

    def test_flat_top_level_fields(self) -> None:
        resolved = appmod.resolve_hyperparameters(
            {"n_epochs": 3, "batch_size": 2, "learning_rate_multiplier": 0.5}
        )
        self.assertEqual(resolved, {
            "n_epochs": 3,
            "batch_size": 2,
            "learning_rate_multiplier": 0.5,
        })

    def test_legacy_hyperparameters_object(self) -> None:
        resolved = appmod.resolve_hyperparameters(
            {"hyperparameters": {"n_epochs": 2, "batch_size": 4}}
        )
        self.assertEqual(resolved["n_epochs"], 2)
        self.assertEqual(resolved["batch_size"], 4)
        self.assertEqual(resolved["learning_rate_multiplier"], 1.0)

    def test_nested_method_wins_over_flat(self) -> None:
        resolved = appmod.resolve_hyperparameters(
            {
                "hyperparameters": {"n_epochs": 1, "batch_size": 1},
                "n_epochs": 2,
                "batch_size": 2,
                "learning_rate_multiplier": 2.0,
                "method": {
                    "type": "supervised",
                    "supervised": {
                        "hyperparameters": {
                            "n_epochs": 5,
                            "batch_size": 8,
                            "learning_rate_multiplier": 0.25,
                        }
                    },
                },
            }
        )
        self.assertEqual(resolved, {
            "n_epochs": 5,
            "batch_size": 8,
            "learning_rate_multiplier": 0.25,
        })

    def test_string_numbers_are_accepted(self) -> None:
        resolved = appmod.resolve_hyperparameters(
            {"n_epochs": "3", "batch_size": "2", "learning_rate_multiplier": "1.5"}
        )
        self.assertEqual(resolved["n_epochs"], 3)
        self.assertEqual(resolved["batch_size"], 2)
        self.assertEqual(resolved["learning_rate_multiplier"], 1.5)

    def test_bad_values_raise_value_error(self) -> None:
        bad_payloads = [
            {"hyperparameters": "auto"},
            {"n_epochs": 0},
            {"n_epochs": 1.5},
            {"n_epochs": True},
            {"batch_size": "many"},
            {"learning_rate_multiplier": 0},
            {"learning_rate_multiplier": -1},
            {"method": "supervised"},
            {"method": {"type": "dpo"}},
            {"method": {"type": "supervised", "supervised": []}},
        ]
        for payload in bad_payloads:
            with self.subTest(payload=payload):
                with self.assertRaises(ValueError):
                    appmod.resolve_hyperparameters(payload)

    def test_create_job_rejects_bad_hyperparameters_as_400(self) -> None:
        # StoreTestCase owns the temporary store; a tiny local one keeps this test simple.
        with tempfile.TemporaryDirectory() as root:
            old = appmod.STORE
            try:
                store = appmod.Store(root)
                appmod.STORE = store
                training_file = store.create_file("train.jsonl", "fine-tune", GOOD_LINE.encode())
                with self.assertRaises(appmod.ApiError) as caught:
                    store.create_job({"training_file": training_file["id"], "n_epochs": 0})
                self.assertEqual(caught.exception.status_code, 400)
                self.assertEqual(caught.exception.code, "invalid_hyperparameters")
                self.assertEqual(store.list_jobs(), [])
            finally:
                appmod.STORE = old


class FineTunedNameTests(unittest.TestCase):
    def test_derivation_uses_suffix_and_short_job_id(self) -> None:
        name = appmod.derive_fine_tuned_model(
            "Qwen/Qwen2.5-1.5B-Instruct", "support", "ftjob-0123456789abcdef01234567"
        )
        self.assertEqual(name, "ft:Qwen-Qwen2.5-1.5B-Instruct:support:01234567")
        self.assertEqual(name.split(":")[0], "ft")

    def test_derivation_falls_back_to_job_id(self) -> None:
        name = appmod.derive_fine_tuned_model("meta-llama/Llama-3.2-1B", None, "ftjob-aabbccddeeff")
        self.assertEqual(name, "ft:meta-llama-Llama-3.2-1B:aabbccdd:aabbccdd")

    def test_suffix_is_sanitised_and_limited(self) -> None:
        name = appmod.derive_fine_tuned_model(
            "base/model", "hello world, this suffix is far too long", "ftjob-12345678"
        )
        self.assertEqual(name, "ft:base-model:hello-world-this-s:12345678")

    def test_blank_suffix_uses_the_job_id(self) -> None:
        name = appmod.derive_fine_tuned_model("base", "   ", "ftjob-deadbeef")
        self.assertEqual(name, "ft:base:deadbeef:deadbeef")


# --------------------------------------------------------------------------------------
# Training-file validation
# --------------------------------------------------------------------------------------


class TargetModuleTests(unittest.TestCase):
    def test_attention_projections_win(self) -> None:
        names = ["model.layers.0.mlp.gate_proj", "model.layers.0.self_attn.q_proj"]
        self.assertEqual(
            appmod.choose_target_modules(names, ["gate_proj", "q_proj"]),
            ["q_proj"],
        )

    def test_first_linear_layers_are_the_fallback(self) -> None:
        names = ["encoder.layer.0.attention.query", "encoder.layer.0.attention.key"]
        self.assertEqual(
            appmod.choose_target_modules(names, ["query", "key", "value", "dense"]),
            ["query", "key", "value", "dense"],
        )

    def test_projection_names_are_used_without_a_module_tree(self) -> None:
        names = ["block.attn.c_proj", "block.mlp.c_proj", "lm_head"]
        self.assertEqual(appmod.choose_target_modules(names), ["c_proj"])


class JsonlValidationTests(unittest.TestCase):
    def test_good_file(self) -> None:
        records, error = appmod.validate_jsonl(GOOD_LINE)
        self.assertIsNone(error)
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["messages"][1]["content"], "4")

    def test_blank_lines_are_ignored(self) -> None:
        records, error = appmod.validate_jsonl("\n" + GOOD_LINE + "\n  \n" + GOOD_LINE + "\n")
        self.assertIsNone(error)
        self.assertEqual(len(records), 2)

    def test_empty_input(self) -> None:
        for value in (None, b"", "", "   \n\n"):
            with self.subTest(value=value):
                records, error = appmod.validate_jsonl(value)
                self.assertEqual(records, [])
                self.assertIsNotNone(error)
                self.assertIn("empty", error.lower())

    def test_line_without_messages(self) -> None:
        records, error = appmod.validate_jsonl('{"prompt": "hi"}\n')
        self.assertEqual(records, [])
        self.assertIn("line 1", error)
        self.assertIn("messages", error)

    def test_line_that_is_not_an_object(self) -> None:
        for line in ('"just a string"\n', "[1, 2, 3]\n", "42\n"):
            with self.subTest(line=line):
                records, error = appmod.validate_jsonl(line)
                self.assertEqual(records, [])
                self.assertIn("not a JSON object", error)

    def test_malformed_json_names_the_line(self) -> None:
        records, error = appmod.validate_jsonl(GOOD_LINE + '{"messages": [}\n')
        self.assertEqual(records, [])
        self.assertIn("line 2", error)
        self.assertIn("not valid JSON", error)

    def test_messages_must_be_a_non_empty_list_of_objects(self) -> None:
        cases = [
            {"messages": []},
            {"messages": "hello"},
            {"messages": [{"role": "user"}]},
            {"messages": [{"role": "user", "content": "hi"}]},
            {"messages": [{"role": "user", "content": 5}]},
        ]
        for record in cases:
            with self.subTest(record=record):
                _, error = appmod.validate_jsonl(json.dumps(record))
                self.assertIsNotNone(error)

    def test_invalid_utf8(self) -> None:
        records, error = appmod.validate_jsonl(b"\xff\xfe")
        self.assertEqual(records, [])
        self.assertIn("UTF-8", error)


# --------------------------------------------------------------------------------------
# Checkpoints and the model catalogue
# --------------------------------------------------------------------------------------


class CheckpointTests(StoreTestCase):
    def test_checkpoint_shape_and_name(self) -> None:
        job = self.make_job()
        self.store().set_status(job["id"], "running")
        self.store().finish_job(job["id"], "succeeded", trained_tokens=128)
        current = self.store().get_job(job["id"])
        checkpoint = appmod.make_checkpoint(current, 7, train_loss=0.25)
        self.assertIsNotNone(checkpoint)
        self.assertEqual(set(checkpoint), {
            "object",
            "id",
            "created_at",
            "fine_tuned_model_checkpoint",
            "step_number",
            "metrics",
            "fine_tuning_job_id",
        })
        self.assertEqual(checkpoint["object"], "fine_tuning.job.checkpoint")
        self.assertTrue(checkpoint["id"].startswith("ftckpt-"))
        self.assertEqual(checkpoint["step_number"], 7)
        self.assertEqual(checkpoint["fine_tuned_model_checkpoint"], current["fine_tuned_model"])
        self.assertEqual(checkpoint["fine_tuning_job_id"], job["id"])
        self.assertEqual(checkpoint["metrics"], {
            "step": 7,
            "train_loss": 0.25,
            "valid_loss": None,
            "full_valid_loss": None,
            "train_mean_token_accuracy": None,
            "valid_mean_token_accuracy": None,
            "full_valid_mean_token_accuracy": None,
        })

    def test_checkpoint_without_a_name_is_not_recorded(self) -> None:
        job = self.make_job()
        current = self.store().get_job(job["id"])
        # A job always derives its future name when it is created, so a normal job has a
        # name even before training runs. A record with neither name field is skipped.
        self.assertIsNotNone(current["_fine_tuned_model"])
        self.assertIsNotNone(appmod.make_checkpoint(current, 1))
        self.assertIsNone(appmod.make_checkpoint({"id": job["id"]}, 1))

    def test_checkpoints_are_listed_in_order_via_the_route(self) -> None:
        job = self.make_job()
        self.store().set_status(job["id"], "running")
        self.store().finish_job(job["id"], "succeeded")
        current = self.store().get_job(job["id"])
        for step in (1, 2):
            self.store().add_checkpoint(job["id"], appmod.make_checkpoint(current, step))
        listed = appmod.list_fine_tuning_checkpoints(job["id"])
        self.assertEqual([item["step_number"] for item in listed["data"]], [1, 2])
        self.assertFalse(listed["has_more"])

    def test_models_list_includes_the_base_and_succeeded_adapter(self) -> None:
        job = self.make_job()
        self.store().finish_job(job["id"], "succeeded")
        current = self.store().get_job(job["id"])
        listing = appmod.list_models()
        ids = {model["id"] for model in listing["data"]}
        self.assertIn(appmod.BASE_MODEL, ids)
        self.assertIn(current["fine_tuned_model"], ids)

    def test_delete_hides_a_fine_tuned_model(self) -> None:
        job = self.make_job()
        self.store().finish_job(job["id"], "succeeded")
        name = self.store().get_job(job["id"])["fine_tuned_model"]
        deleted = appmod.delete_model(name)
        self.assertEqual(deleted["deleted"], True)
        self.assertTrue(self.store().get_job(job["id"])["_deleted"])
        self.assertNotIn(name, {model["id"] for model in appmod.list_models()["data"]})
        self.assertApiError(lambda: appmod.delete_model(name), 404, "model_not_found")
        self.assertApiError(lambda: appmod.delete_model(appmod.BASE_MODEL), 400, "model_not_deletable")


# --------------------------------------------------------------------------------------
# Files
# --------------------------------------------------------------------------------------


class FileStoreTests(StoreTestCase):
    def test_create_get_list_and_read_content(self) -> None:
        meta = self.upload("first\n", "first.jsonl")
        self.assertEqual(meta["object"], "file")
        self.assertEqual(meta["purpose"], "fine-tune")
        self.assertEqual(meta["bytes"], len(b"first\n"))
        self.assertEqual(self.store().file_content(meta["id"]), b"first\n")
        self.assertEqual(self.store().get_file(meta["id"])["filename"], "first.jsonl")
        self.assertEqual(self.store().list_files()[0]["id"], meta["id"])

    def test_upload_route_reads_the_multipart_file(self) -> None:
        class Upload:
            filename = "route.jsonl"

            async def read(self):
                return GOOD_LINE.encode()

        meta = asyncio.run(appmod.create_file(Upload(), "fine-tune"))
        self.assertEqual(meta["filename"], "route.jsonl")
        self.assertEqual(meta["purpose"], "fine-tune")
        response = appmod.get_file_content(meta["id"])
        self.assertEqual(response.media_type, "application/jsonl")
        self.assertEqual(response.content, GOOD_LINE.encode())

    def test_empty_upload_is_rejected(self) -> None:
        class Upload:
            filename = "empty.jsonl"

            async def read(self):
                return b""

        self.assertApiError(lambda: asyncio.run(appmod.create_file(Upload(), "fine-tune")), 400, "invalid_file")


# --------------------------------------------------------------------------------------
# Path safety and ids
# --------------------------------------------------------------------------------------


class PathSafetyTests(StoreTestCase):
    def test_valid_ids_receive_generated_names(self) -> None:
        ids = {appmod.new_id("ftjob") for _ in range(100)}
        self.assertEqual(len(ids), 100)
        for value in ids:
            self.assertTrue(value.startswith("ftjob-"))
            self.assertIsNotNone(appmod._SAFE_ID.fullmatch(value))

    def test_traversal_is_rejected_by_id_validation(self) -> None:
        for bad in ("../../etc", "/etc/passwd", "..", ".", "a/b", "a\\b", "a b", "", None):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    appmod._require_id(bad, "job")

    def test_job_paths_cannot_escape_the_state_directory(self) -> None:
        self.assertIsNone(self.store().get_job("../../etc"))
        self.assertIsNone(self.store().get_job("../job.json"))
        with self.assertRaises(ValueError):
            self.store().job_dir("../../etc")
        with self.assertRaises(ValueError):
            self.store().file_meta_path("../../etc")
        self.assertIsNone(self.store().get_file("../../etc"))

    def test_safe_paths_stay_under_the_store_root(self) -> None:
        job = self.make_job()
        path = self.store().job_dir(job["id"]).resolve()
        root = self.store().jobs.resolve()
        self.assertEqual(path.parent, root)

    def test_unknown_job_routes_are_404(self) -> None:
        self.assertApiError(lambda: appmod.get_fine_tuning_job("ftjob-nope"), 404, "job_not_found")
        self.assertApiError(
            lambda: appmod.list_fine_tuning_events("ftjob-nope"), 404, "job_not_found"
        )
        self.assertApiError(
            lambda: appmod.list_fine_tuning_checkpoints("ftjob-nope"), 404, "job_not_found"
        )


# --------------------------------------------------------------------------------------
# Background validation and deployment constants
# --------------------------------------------------------------------------------------


class BackgroundValidationTests(StoreTestCase):
    def test_bad_training_file_fails_the_job_instead_of_raising(self) -> None:
        training = self.store().create_file(
            "bad.jsonl", "fine-tune", b'{"prompt": "no messages"}\n'
        )
        job = self.store().create_job({"training_file": training["id"]})
        appmod._run_job(job["id"])
        failed = self.store().get_job(job["id"])
        self.assertEqual(failed["status"], "failed")
        self.assertEqual(failed["error"]["code"], "invalid_training_file")
        self.assertFalse((self.store().jobs / "ACTIVE.json").exists())

    def test_training_file_must_exist(self) -> None:
        self.assertApiError(
            lambda: self.store().create_job({"training_file": "file-nope"}),
            400,
            "invalid_training_file",
        )

    def test_fine_tuning_a_fine_tuned_model_is_refused(self) -> None:
        self.assertApiError(
            lambda: self.make_job(model="ft:some:model:abc"),
            400,
            "unsupported_model",
        )


class DeploymentTests(unittest.TestCase):
    def test_gpu_and_container_constants(self) -> None:
        self.assertEqual(appmod.GPU, "RTX-PRO-6000")
        self.assertEqual(appmod.SCALEDOWN_WINDOW, 1200)
        self.assertEqual(appmod.STATE_DIR, "/state")

    def test_modal_app_and_volume_are_constructed(self) -> None:
        self.assertEqual(appmod.app.name, "manarouter-finetune")
        self.assertEqual(appmod.volume.name, "manarouter-finetune-state")
        self.assertIn("RTX-PRO-6000", appmod.app.cls_kwargs.get("gpu", ""))

    def test_health_endpoint_does_not_touch_the_store(self) -> None:
        before = appmod.STORE
        body = appmod.healthz()
        self.assertEqual(body["status"], "ok")
        self.assertEqual(body["gpu"], "RTX-PRO-6000")
        self.assertIs(appmod.STORE, before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
