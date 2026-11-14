"""A reference OpenAI-compatible fine-tuning provider, deployed as a Modal app.

This app is deliberately small and readable: it is one Modal container with one GPU, a
FastAPI surface that mirrors the parts of OpenAI's fine-tuning API that a router needs,
and a state directory on a Modal Volume. It is not a production fine-tuning platform.

The HTTP API is described in ``README.md`` and implemented below. All state lives under
``STATE_DIR`` (``/state`` in the deployed app):

    files/<file_id>.jsonl              uploaded training files
    files/<file_id>.json               their metadata
    jobs/<job_id>/job.json             the job object (internal keys start with ``_``)
    jobs/<job_id>/events.jsonl         append-only event log
    jobs/<job_id>/checkpoints.jsonl    append-only checkpoint log
    jobs/<job_id>/control.json         {"pause": bool, "cancel": bool}
    jobs/<job_id>/adapter/             the trained LoRA adapter, after success
    jobs/ACTIVE.json                   the job currently holding the GPU

``control.json`` and ``ACTIVE.json`` are on the Volume rather than in memory because
Modal may run more than one container for this app: a request that arrives while the
training container is busy can be served by a second container, and pause/cancel must
work there. Every write is followed by ``volume.commit()`` and every control read does
``volume.reload()`` first, so the trainer sees changes made by another container.
"""

from __future__ import annotations

import gc
import json
import logging
import os
import re
import shutil
import threading
import time
import uuid
from pathlib import Path
from typing import Any

import modal
from fastapi import FastAPI, File, Form, HTTPException, Request, UploadFile
from fastapi.responses import JSONResponse, Response

logger = logging.getLogger("manarouter.finetune")
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")

# --------------------------------------------------------------------------------------
# Deployment constants
# --------------------------------------------------------------------------------------

APP_NAME = "manarouter-finetune"
GPU = "RTX-PRO-6000"

# A small instruct model that fits comfortably in the 96 GB card and is not gated. Set
# BASE_MODEL at deploy time (or per job with the ``model`` field) to use another model.
BASE_MODEL = os.environ.get("BASE_MODEL", "Qwen/Qwen2.5-1.5B-Instruct")
MAX_SEQ_LENGTH = int(os.environ.get("MAX_SEQ_LENGTH", "1024"))

# The card is billed while the container exists, including the scale-down tail. 1200 s is
# Modal's maximum for scaledown_window, and keeps the container alive across a training
# run that is being polled. See README.md for the honest idle/cost story.
SCALEDOWN_WINDOW = 1200

STATE_DIR = os.environ.get("STATE_DIR", "/state")
DEFAULT_LIMIT = 20
MAX_LIMIT = 100

TERMINAL_STATUSES = frozenset({"succeeded", "failed", "cancelled"})
ACTIVE_STATUSES = frozenset({"validating_files", "queued", "running"})

# A job whose job.json has not been touched for this long is treated as orphaned by a
# container that starts later. A running trainer touches its job once per optimizer step,
# so this only fires after a container death or a truly wedged step.
ACTIVE_STALE_SECONDS = 60 * 30

# --------------------------------------------------------------------------------------
# Errors
# --------------------------------------------------------------------------------------


class ApiError(Exception):
    """An error that becomes an OpenAI-shaped JSON error response."""

    def __init__(
        self,
        status_code: int,
        message: str,
        *,
        code: str | None = None,
        param: str | None = None,
        error_type: str = "invalid_request_error",
    ) -> None:
        super().__init__(message)
        self.status_code = status_code
        self.message = message
        self.code = code
        self.param = param
        self.error_type = error_type

    def body(self) -> dict[str, Any]:
        return {
            "error": {
                "message": self.message,
                "type": self.error_type,
                "param": self.param,
                "code": self.code,
            }
        }


def _error_response(status_code: int, exc: ApiError) -> JSONResponse:
    return JSONResponse(status_code=status_code, content=exc.body())


# --------------------------------------------------------------------------------------
# Pure helpers (kept free of Modal, FastAPI and torch so they are testable on a CPU box)
# --------------------------------------------------------------------------------------


def new_id(prefix: str) -> str:
    """Return a new opaque id such as ``ftjob-1f0c...``."""
    return f"{prefix}-{uuid.uuid4().hex[:24]}"


_SAFE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}")


def _require_id(value: Any, kind: str) -> str:
    """Validate a client-supplied id before it is used as a path component.

    ``Path(root) / "../../etc"`` escapes the root; ``Path(root) / "/etc"`` replaces it.
    Both are rejected here, and the callers also resolve the final path and check that it
    is still inside the root, so a future edit cannot reintroduce the traversal.
    """
    if not isinstance(value, str) or not value:
        raise ValueError(f"{kind} id must be a non-empty string")
    if not _SAFE_ID.fullmatch(value):
        raise ValueError(f"{kind} id contains characters that are not allowed in a path")
    return value


def validate_jsonl(data: bytes | str | None) -> tuple[list[dict[str, Any]], str | None]:
    """Parse and check an uploaded JSONL fine-tuning file.

    Returns ``(records, None)`` on success and ``([], message)`` on the first problem.
    Each non-blank line must be a JSON object with a non-empty ``messages`` array; every
    message must have string ``role`` and ``content`` fields, and at least one message
    must be from the assistant. That is stricter than the OpenAI schema (which also
    accepts tool calls) but it is the subset this reference trainer can render.
    """
    if data is None:
        return [], "the training file is empty"
    if isinstance(data, bytes):
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError as exc:
            return [], f"the training file is not valid UTF-8: {exc}"
    else:
        text = data
    if not text.strip():
        return [], "the training file is empty"

    records: list[dict[str, Any]] = []
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as exc:
            return [], f"line {number} is not valid JSON: {exc.msg} (column {exc.colno})"
        if not isinstance(record, dict):
            return [], f"line {number} is not a JSON object"
        messages = record.get("messages")
        if not isinstance(messages, list) or not messages:
            return [], f'line {number} has no non-empty "messages" array'
        for index, message in enumerate(messages):
            if not isinstance(message, dict):
                return [], f"line {number} message {index} is not a JSON object"
            if not isinstance(message.get("role"), str):
                return [], f'line {number} message {index} has no string "role"'
            if not isinstance(message.get("content"), str):
                return [], f'line {number} message {index} has no string "content"'
        if not any(message["role"] == "assistant" for message in messages):
            return [], f"line {number} has no assistant message to train on"
        records.append(record)

    if not records:
        return [], "the training file has no examples"
    return records, None


def _as_int(value: Any, name: str) -> int:
    if isinstance(value, bool):
        raise ValueError(f"{name} must be an integer, not a boolean")
    try:
        number = int(value)
    except (TypeError, ValueError):
        raise ValueError(f"{name} must be an integer") from None
    if isinstance(value, float) and value != number:
        raise ValueError(f"{name} must be a whole number")
    return number


def _as_float(value: Any, name: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{name} must be a number, not a boolean")
    try:
        number = float(value)
    except (TypeError, ValueError):
        raise ValueError(f"{name} must be a number") from None
    if number != number or number in (float("inf"), float("-inf")):
        raise ValueError(f"{name} must be finite")
    return number


def resolve_hyperparameters(payload: dict[str, Any]) -> dict[str, Any]:
    """Resolve flat and nested hyperparameters, with the nested form winning.

    OpenAI accepted flat ``n_epochs``/``batch_size``/``learning_rate_multiplier`` fields
    before ``method.supervised.hyperparameters`` existed, and clients still send both.
    The nested value wins when both are present; missing values use this reference
    provider's defaults (one epoch, batch size one, multiplier one, which means ``lr``
    is ``2e-4``).
    """
    nested: dict[str, Any] = {}
    method = payload.get("method")
    if method is not None:
        if not isinstance(method, dict):
            raise ValueError("method must be a JSON object")
        method_type = method.get("type", "supervised")
        if method_type != "supervised":
            raise ValueError('only method.type "supervised" is supported')
        supervised = method.get("supervised")
        if supervised is None:
            supervised = {}
        if not isinstance(supervised, dict):
            raise ValueError("method.supervised must be a JSON object")
        nested = supervised.get("hyperparameters")
        if nested is None:
            nested = {}
        if not isinstance(nested, dict):
            raise ValueError("method.supervised.hyperparameters must be a JSON object")

    merged: dict[str, Any] = {}
    # OpenAI clients send the older flat shape as ``"hyperparameters": {...}``. Accept it,
    # then top-level fields, then the nested object, each layer winning over the last.
    flat = payload.get("hyperparameters")
    if flat is not None:
        if not isinstance(flat, dict):
            raise ValueError("hyperparameters must be a JSON object")
        merged.update(flat)
    for key in ("n_epochs", "batch_size", "learning_rate_multiplier"):
        if key in payload and payload[key] is not None:
            merged[key] = payload[key]
    merged.update(nested)

    n_epochs = _as_int(merged.get("n_epochs", 1), "n_epochs")
    batch_size = _as_int(merged.get("batch_size", 1), "batch_size")
    multiplier = _as_float(merged.get("learning_rate_multiplier", 1.0), "learning_rate_multiplier")
    if n_epochs < 1:
        raise ValueError("n_epochs must be at least 1")
    if batch_size < 1:
        raise ValueError("batch_size must be at least 1")
    if multiplier <= 0:
        raise ValueError("learning_rate_multiplier must be greater than 0")
    return {
        "n_epochs": n_epochs,
        "batch_size": batch_size,
        "learning_rate_multiplier": multiplier,
    }


def derive_fine_tuned_model(
    base_model: str, suffix: str | None = None, job_id: str = ""
) -> str:
    """Build the OpenAI-style fine-tuned model name.

    ``ft:<base-model>:<suffix>:<short-id>``. The base model's ``/`` becomes ``-`` so the
    name is safe in a URL path and in a provider configuration; the suffix is limited to
    18 characters and the short id is the last eight characters of the job id.
    """
    base = str(base_model or BASE_MODEL).strip() or BASE_MODEL
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", base.replace("/", "-")).strip("-.") or "model"

    if suffix is not None:
        suffix = re.sub(r"[^A-Za-z0-9._-]+", "-", str(suffix).strip()).strip("-.")[:18]
    else:
        suffix = ""
    if not suffix:
        suffix = (job_id.rsplit("-", 1)[-1] if job_id else uuid.uuid4().hex[:8])[:8]

    short = (job_id.rsplit("-", 1)[-1] if job_id else uuid.uuid4().hex[:8])[:8]
    return f"ft:{slug}:{suffix}:{short}"


def make_checkpoint(
    job: dict[str, Any],
    step: int,
    train_loss: float | None = None,
    valid_loss: float | None = None,
) -> dict[str, Any] | None:
    """Build the checkpoint object for ``GET .../checkpoints``.

    OpenAI's documented example repeats the ``object`` key; a Python dict cannot carry
    the same key twice, so this object has ``object`` exactly once and every other field
    from the documented example.
    """
    name = job.get("fine_tuned_model") or job.get("_fine_tuned_model")
    if not name:
        return None
    step = int(step)
    return {
        "object": "fine_tuning.job.checkpoint",
        "id": new_id("ftckpt"),
        "created_at": int(time.time()),
        "fine_tuned_model_checkpoint": name,
        "step_number": step,
        # This reference trainer does not compute per-token accuracy. The fields are
        # present and null rather than omitted, so an OpenAI client sees the full shape.
        "metrics": {
            "step": step,
            "train_loss": train_loss,
            "valid_loss": valid_loss,
            "full_valid_loss": None,
            "train_mean_token_accuracy": None,
            "valid_mean_token_accuracy": None,
            "full_valid_mean_token_accuracy": None,
        },
        "fine_tuning_job_id": job["id"],
    }


def choose_target_modules(
    names: list[str], linear_leaves: list[str] | None = None
) -> list[str]:
    """Pick LoRA target modules from the model's module names.

    Attention projections first. When an architecture has no ``q_proj``/``k_proj``/
    ``v_proj``/``o_proj`` leaves, the first linear layers are used instead. ``linear_leaves``
    is that ordered list, supplied by the trainer from the live module tree; without it a
    projection-name heuristic is used, which also keeps the helper easy to unit test.
    """
    def leaf(name: str) -> str:
        return name.rsplit(".", 1)[-1]

    leaves = {leaf(name) for name in names}
    attention = [name for name in ("q_proj", "k_proj", "v_proj", "o_proj") if name in leaves]
    if attention:
        return attention

    if linear_leaves:
        chosen: list[str] = []
        for candidate in linear_leaves:
            if candidate and candidate not in chosen:
                chosen.append(candidate)
            if len(chosen) == 4:
                break
        if chosen:
            return chosen

    projections: list[str] = []
    for name in names:
        candidate = leaf(name)
        if candidate.endswith("_proj") and candidate not in projections:
            projections.append(candidate)
    if projections:
        return projections[:4]
    return ["q_proj", "k_proj", "v_proj", "o_proj"]


def _fallback_chat_text(record: dict[str, Any]) -> str:
    parts = []
    for message in record.get("messages", []):
        content = message.get("content")
        if isinstance(content, str) and content:
            parts.append(f"{message.get('role', 'user')}: {content}")
    return "\n".join(parts)


def render_example(tokenizer: Any, record: dict[str, Any]) -> str | None:
    """Render one conversation to text with the model's own chat template."""
    messages = record.get("messages") or []
    try:
        text = tokenizer.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=False
        )
    except Exception:
        try:
            text = tokenizer.apply_chat_template(messages, tokenize=False)
        except Exception:
            text = None
    if isinstance(text, str) and text.strip():
        return text
    fallback = _fallback_chat_text(record)
    return fallback if fallback.strip() else None


def public_job(job: dict[str, Any]) -> dict[str, Any]:
    """Strip the provider's internal bookkeeping before returning a job to a client."""
    return {key: value for key, value in job.items() if not key.startswith("_")}


def _is_stale(job: dict[str, Any], threshold: float = ACTIVE_STALE_SECONDS) -> bool:
    timestamp = job.get("_updated_at") or job.get("created_at") or 0
    try:
        return (time.time() - float(timestamp)) > threshold
    except (TypeError, ValueError):
        return True


def _clamp_limit(limit: Any) -> int:
    try:
        value = int(limit)
    except (TypeError, ValueError):
        return DEFAULT_LIMIT
    return max(1, min(value, MAX_LIMIT))


def _after_index(items: list[dict[str, Any]], after: Any) -> int:
    if not isinstance(after, str) or not after:
        return 0
    for index, item in enumerate(items):
        if item.get("id") == after:
            return index + 1
    # An unknown cursor is treated as "from the beginning" rather than as an error, which
    # is what a client that lost its cursor needs. It is also cheap to reason about.
    return 0


def _page(
    items: list[dict[str, Any]], limit: Any, after: Any
) -> tuple[list[dict[str, Any]], bool]:
    size = _clamp_limit(limit)
    start = _after_index(items, after)
    page = items[start : start + size]
    return page, start + len(page) < len(items)


# --------------------------------------------------------------------------------------
# The on-Volume store
# --------------------------------------------------------------------------------------


class Store:
    """All mutable state, rooted at ``STATE_DIR``.

    Constructing a Store does not create directories: importing this module on a CPU-only
    development machine must not try to write ``/state``. Directories are created on the
    first write. The optional ``commit``/``reload`` callables are the Modal Volume's
    persistence calls; tests pass ``None`` (or counters) and run against a temporary
    directory.
    """

    def __init__(self, root: str | Path, commit=None, reload=None) -> None:
        self.root = Path(root)
        self.files = self.root / "files"
        self.jobs = self.root / "jobs"
        self.active_path = self.jobs / "ACTIVE.json"
        self._commit = commit
        self._reload = reload
        self._lock = threading.RLock()

    def bind(self, commit, reload) -> None:
        """Attach the Modal Volume's persistence calls once the container starts."""
        with self._lock:
            self._commit = commit
            self._reload = reload

    # -- persistence ------------------------------------------------------------------

    def commit(self) -> None:
        if self._commit is None:
            return
        try:
            self._commit()
        except Exception:
            logger.exception("volume commit failed")

    def reload(self) -> None:
        if self._reload is None:
            return
        try:
            self._reload()
        except Exception:
            logger.exception("volume reload failed")

    def _ensure_dir(self, path: Path) -> None:
        path.mkdir(parents=True, exist_ok=True)

    def _ensure_files_dir(self) -> None:
        self._ensure_dir(self.files)

    def _ensure_jobs_dir(self) -> None:
        self._ensure_dir(self.jobs)

    def _ensure_job_dir(self, job_id: str) -> Path:
        path = self.job_dir(job_id)
        self._ensure_dir(path)
        return path

    @staticmethod
    def _read_json(path: Path) -> Any:
        try:
            with path.open("r", encoding="utf-8") as handle:
                return json.load(handle)
        except (OSError, json.JSONDecodeError, ValueError):
            return None

    @staticmethod
    def _write_json(path: Path, value: Any) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(path.name + ".tmp")
        with tmp.open("w", encoding="utf-8") as handle:
            json.dump(value, handle, indent=2, sort_keys=True)
            handle.write("\n")
        os.replace(tmp, path)

    @staticmethod
    def _append_line(path: Path, value: Any) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        line = json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
        with path.open("a", encoding="utf-8") as handle:
            handle.write(line)

    def _read_lines(self, path: Path) -> list[dict[str, Any]]:
        try:
            with path.open("r", encoding="utf-8") as handle:
                lines = handle.readlines()
        except OSError:
            return []
        values = []
        for line in lines:
            line = line.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                values.append(value)
        return values

    # -- path safety ------------------------------------------------------------------

    def _safe_path(self, directory: Path, name: str, kind: str) -> Path:
        # ``name`` is a single file or directory component here (callers validate ids with
        # ``_require_id`` before adding a suffix such as ``.json``), so this only rejects
        # separators and the two directory aliases. The resolved-parent check below is the
        # second line of defence against a traversal.
        component = str(name or "")
        if (
            not component
            or component in (".", "..")
            or "/" in component
            or "\\" in component
            or "\x00" in component
        ):
            raise ValueError(f"{kind} id is not a valid path component")
        base = directory.resolve()
        path = (base / component).resolve()
        if path.parent != base:
            raise ValueError(f"{kind} id escapes its directory")
        return path

    def file_meta_path(self, file_id: str) -> Path:
        return self._safe_path(self.files, f"{_require_id(file_id, 'file')}.json", "file")

    def file_data_path(self, file_id: str) -> Path:
        return self._safe_path(self.files, f"{_require_id(file_id, 'file')}.jsonl", "file")

    def job_dir(self, job_id: str) -> Path:
        return self._safe_path(self.jobs, job_id, "job")

    # -- files --------------------------------------------------------------------------

    def create_file(self, filename: str, purpose: str, data: bytes) -> dict[str, Any]:
        file_id = new_id("file")
        created_at = int(time.time())
        meta = {
            "object": "file",
            "id": file_id,
            "bytes": len(data),
            "created_at": created_at,
            "filename": filename,
            "purpose": purpose,
            "status": "processed",
            "status_details": None,
        }
        with self._lock:
            self._ensure_files_dir()
            self.file_data_path(file_id).write_bytes(data)
            self._write_json(self.file_meta_path(file_id), meta)
            self.commit()
        return meta

    def get_file(self, file_id: str) -> dict[str, Any] | None:
        try:
            meta = self._read_json(self.file_meta_path(file_id))
        except ValueError:
            return None
        return meta if isinstance(meta, dict) else None

    def file_content(self, file_id: str) -> bytes | None:
        try:
            return self.file_data_path(file_id).read_bytes()
        except (OSError, ValueError):
            return None

    def list_files(self) -> list[dict[str, Any]]:
        with self._lock:
            if not self.files.exists():
                return []
            try:
                paths = list(self.files.glob("*.json"))
            except OSError:
                return []
            metas = []
            for path in paths:
                meta = self._read_json(path)
                if isinstance(meta, dict) and isinstance(meta.get("id"), str):
                    metas.append(meta)
        metas.sort(key=lambda item: (item.get("created_at", 0), item.get("id", "")), reverse=True)
        return metas

    # -- job storage --------------------------------------------------------------------

    def _all_jobs(self) -> list[dict[str, Any]]:
        with self._lock:
            if not self.jobs.exists():
                return []
            try:
                children = list(self.jobs.iterdir())
            except OSError:
                return []
            jobs = []
            for child in children:
                if not child.is_dir():
                    continue
                job = self._read_json(child / "job.json")
                if isinstance(job, dict) and isinstance(job.get("id"), str):
                    jobs.append(job)
        return jobs

    def get_job(self, job_id: str) -> dict[str, Any] | None:
        try:
            job = self._read_json(self.job_dir(job_id) / "job.json")
        except ValueError:
            return None
        return job if isinstance(job, dict) else None

    def list_jobs(self) -> list[dict[str, Any]]:
        jobs = self._all_jobs()
        # created_at has second resolution, so two jobs made in the same second need the
        # nanosecond stamp (or their random ids) to break the tie deterministically.
        jobs.sort(
            key=lambda item: (
                item.get("created_at", 0),
                item.get("_created_ns", 0),
                item.get("id", ""),
            ),
            reverse=True,
        )
        return jobs

    def update_job(self, job_id: str, **fields: Any) -> dict[str, Any]:
        with self._lock:
            job = self.get_job(job_id)
            if job is None:
                raise KeyError(f"no such job: {job_id}")
            job.update(fields)
            job["_updated_at"] = int(time.time())
            self._write_json(self.job_dir(job_id) / "job.json", job)
            self.commit()
            return job

    def set_status(self, job_id: str, status: str) -> dict[str, Any]:
        with self._lock:
            job = self.get_job(job_id)
            if job is None:
                raise KeyError(f"no such job: {job_id}")
            if job.get("status") in TERMINAL_STATUSES:
                return job
            job["status"] = status
            job["_updated_at"] = int(time.time())
            self._write_json(self.job_dir(job_id) / "job.json", job)
            self.commit()
            return job

    def touch_job(self, job_id: str) -> dict[str, Any] | None:
        """Refresh ``_updated_at`` so another container can tell this job is alive."""
        with self._lock:
            job = self.get_job(job_id)
            if job is None:
                return None
            job["_updated_at"] = int(time.time())
            self._write_json(self.job_dir(job_id) / "job.json", job)
            self.commit()
            return job

    def finish_job(
        self,
        job_id: str,
        status: str,
        *,
        error: dict[str, Any] | None = None,
        trained_tokens: int | None = None,
    ) -> dict[str, Any]:
        if status not in TERMINAL_STATUSES:
            raise ValueError(f"not a terminal status: {status}")
        with self._lock:
            job = self.get_job(job_id)
            if job is None:
                raise KeyError(f"no such job: {job_id}")
            if job.get("status") in TERMINAL_STATUSES:
                return job
            job["status"] = status
            job["finished_at"] = int(time.time())
            job["_updated_at"] = job["finished_at"]
            if status == "succeeded":
                # The name is derived when the job is created (and is what the checkpoint
                # references); only publish it once there is a trained adapter to serve.
                job["fine_tuned_model"] = (
                    job.get("fine_tuned_model") or job.get("_fine_tuned_model")
                )
            if error is not None:
                job["error"] = error
            if trained_tokens is not None:
                job["trained_tokens"] = int(trained_tokens)
            self._write_json(self.job_dir(job_id) / "job.json", job)

            if status == "succeeded":
                message = f"Job {job_id} succeeded."
            elif status == "cancelled":
                message = f"Job {job_id} was cancelled."
            else:
                message = (error or {}).get("message") or f"Job {job_id} failed."
            self.append_event(
                job_id,
                "error" if status == "failed" else "info",
                message,
                {
                    "status": status,
                    "trained_tokens": job.get("trained_tokens"),
                    **({"error": error} if error else {}),
                },
            )
            self.clear_active(job_id)
            self.commit()
            return job

    # -- events and checkpoints ---------------------------------------------------------

    def append_event(
        self,
        job_id: str,
        level: str,
        message: str,
        data: dict[str, Any] | None = None,
        *,
        event_type: str | None = None,
    ) -> dict[str, Any]:
        # ``level`` is one of info/warn/error; a metrics event is an info event whose
        # ``type`` is ``"metrics"``, not a level of its own.
        if level == "metrics":
            level = "info"
            event_type = event_type or "metrics"
        if level not in ("info", "warn", "error"):
            level = "info"
        event = {
            "object": "fine_tuning.job.event",
            "id": new_id("ftevent"),
            "created_at": int(time.time()),
            "level": level,
            "message": message,
            "type": event_type or "message",
            "data": data or {},
        }
        with self._lock:
            self._ensure_job_dir(job_id)
            self._append_line(self.job_dir(job_id) / "events.jsonl", event)
            self.commit()
        return event

    def list_events(self, job_id: str) -> list[dict[str, Any]]:
        try:
            path = self.job_dir(job_id) / "events.jsonl"
        except ValueError:
            return []
        return self._read_lines(path)

    def add_checkpoint(self, job_id: str, checkpoint: dict[str, Any]) -> dict[str, Any]:
        with self._lock:
            self._ensure_job_dir(job_id)
            self._append_line(self.job_dir(job_id) / "checkpoints.jsonl", checkpoint)
            self.commit()
        return checkpoint

    def list_checkpoints(self, job_id: str) -> list[dict[str, Any]]:
        try:
            path = self.job_dir(job_id) / "checkpoints.jsonl"
        except ValueError:
            return []
        return self._read_lines(path)

    # -- control file ------------------------------------------------------------------

    def write_control(self, job_id: str, updates: dict[str, bool]) -> dict[str, bool]:
        with self._lock:
            self._ensure_job_dir(job_id)
            path = self.job_dir(job_id) / "control.json"
            current = self._read_json(path)
            control = {
                "pause": bool(current.get("pause", False)) if isinstance(current, dict) else False,
                "cancel": bool(current.get("cancel", False)) if isinstance(current, dict) else False,
            }
            for key in ("pause", "cancel"):
                if key in updates:
                    control[key] = bool(updates[key])
            self._write_json(path, control)
            self.commit()
            return control

    def read_control(self, job_id: str, fresh: bool = False) -> dict[str, bool]:
        with self._lock:
            if fresh:
                self.commit()
                self.reload()
            try:
                path = self.job_dir(job_id) / "control.json"
            except ValueError:
                return {"pause": False, "cancel": False}
            current = self._read_json(path)
        if not isinstance(current, dict):
            return {"pause": False, "cancel": False}
        return {
            "pause": bool(current.get("pause", False)),
            "cancel": bool(current.get("cancel", False)),
        }

    # -- active job marker ----------------------------------------------------------------

    def set_active(self, job_id: str) -> None:
        with self._lock:
            self._ensure_jobs_dir()
            self._write_json(self.active_path, {"job_id": job_id, "created_at": int(time.time())})
            self.commit()

    def try_acquire_active(self, job_id: str) -> bool:
        """Atomically claim the GPU with an ``O_EXCL`` create of ``jobs/ACTIVE.json``.

        A plain read-then-write would let two containers both see "no active job" and
        both start training. Creating the marker exclusively makes exactly one of them
        win; the loser gets a 409. If the volume does not support ``O_EXCL`` (unusual),
        fall back to the plain write so the app still works, with the in-process lock
        protecting same-container requests.
        """
        with self._lock:
            self._ensure_jobs_dir()
            try:
                fd = os.open(self.active_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
            except FileExistsError:
                return False
            except OSError:
                logger.warning("Volume does not support O_EXCL for the active marker; falling back")
                self._write_json(
                    self.active_path, {"job_id": job_id, "created_at": int(time.time())}
                )
                self.commit()
                return True
            try:
                with os.fdopen(fd, "w", encoding="utf-8") as handle:
                    json.dump({"job_id": job_id, "created_at": int(time.time())}, handle)
                    handle.flush()
                    os.fsync(handle.fileno())
            except Exception:
                try:
                    self.active_path.unlink()
                except OSError:
                    pass
                raise
            self.commit()
            return True

    def _discard_job(self, job_id: str) -> None:
        """Remove a job created just before losing the active-marker race."""
        with self._lock:
            try:
                shutil.rmtree(self.job_dir(job_id))
            except FileNotFoundError:
                pass
            except OSError:
                logger.exception("Could not remove discarded job %s", job_id)
            self.clear_active(job_id)
            self.commit()

    def read_active(self, fresh: bool = False) -> dict[str, Any] | None:
        with self._lock:
            if fresh:
                self.commit()
                self.reload()
            value = self._read_json(self.active_path)
        return value if isinstance(value, dict) else None

    def clear_active(self, job_id: str | None = None) -> None:
        with self._lock:
            current = self._read_json(self.active_path)
            if job_id and (not isinstance(current, dict) or current.get("job_id") != job_id):
                return
            try:
                self.active_path.unlink()
            except FileNotFoundError:
                return
            self.commit()

    # -- job lifecycle ------------------------------------------------------------------

    def create_job(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Validate a create request, persist a ``validating_files`` job, return it.

        The caller starts the training thread. Refusing a second job here (rather than in
        the endpoint) keeps the check next to the marker it protects.
        """
        if not isinstance(payload, dict):
            raise ApiError(400, "The request body must be a JSON object.", code="invalid_request")

        training_file = payload.get("training_file")
        if not isinstance(training_file, str) or not training_file.strip():
            raise ApiError(
                400,
                "training_file is required.",
                code="invalid_request",
                param="training_file",
            )
        training_file = training_file.strip()
        if self.get_file(training_file) is None:
            raise ApiError(
                400,
                f"Training file {training_file!r} was not found. Upload it with POST /v1/files first.",
                code="invalid_training_file",
                param="training_file",
            )

        validation_file = payload.get("validation_file")
        if validation_file is not None:
            if not isinstance(validation_file, str) or self.get_file(validation_file) is None:
                raise ApiError(
                    400,
                    f"Validation file {validation_file!r} was not found.",
                    code="invalid_validation_file",
                    param="validation_file",
                )

        model = payload.get("model", BASE_MODEL)
        if model is None:
            model = BASE_MODEL
        if not isinstance(model, str) or not model.strip():
            raise ApiError(400, "model must be a non-empty string.", param="model")
        model = model.strip()
        if model.startswith("ft:"):
            raise ApiError(
                400,
                "Fine-tuning a fine-tuned model is not supported by this reference provider.",
                code="unsupported_model",
                param="model",
            )

        try:
            hyperparameters = resolve_hyperparameters(payload)
        except ValueError as exc:
            raise ApiError(
                400, str(exc), code="invalid_hyperparameters", param="hyperparameters"
            ) from None

        suffix = payload.get("suffix")
        if suffix is not None and not isinstance(suffix, str):
            raise ApiError(400, "suffix must be a string.", param="suffix")

        seed = payload.get("seed")
        if seed is not None:
            try:
                seed = _as_int(seed, "seed")
            except ValueError as exc:
                raise ApiError(400, str(exc), param="seed") from None

        metadata = payload.get("metadata")
        if metadata is not None and not isinstance(metadata, dict):
            raise ApiError(400, "metadata must be a JSON object.", param="metadata")

        integrations = payload.get("integrations")
        if integrations is not None and not isinstance(integrations, list):
            raise ApiError(400, "integrations must be a JSON array.", param="integrations")

        # The active-job check lives inside the lock below, next to the marker it protects.

        job_id = new_id("ftjob")
        created_at = int(time.time())
        fine_tuned_model = derive_fine_tuned_model(model, suffix, job_id)
        job: dict[str, Any] = {
            "object": "fine_tuning.job",
            "id": job_id,
            "created_at": created_at,
            "finished_at": None,
            "model": model,
            "fine_tuned_model": None,
            "organization_id": "org-manarouter",
            "result_files": [],
            "status": "validating_files",
            "validation_file": validation_file,
            "training_file": training_file,
            "hyperparameters": hyperparameters,
            "trained_tokens": None,
            "error": None,
            "integrations": integrations or [],
            "seed": seed,
            "estimated_finish": None,
            "metadata": metadata,
            "method": {
                "type": "supervised",
                "supervised": {"hyperparameters": dict(hyperparameters)},
            },
            # Internal fields; public_job() removes them.
            "_fine_tuned_model": fine_tuned_model,
            "_updated_at": created_at,
            "_created_ns": time.time_ns(),
            "_last_step": 0,
            "_last_loss": None,
            "_deleted": False,
        }

        with self._lock:
            # Refuse a second job before touching the GPU. The marker lives on the Volume
            # so a second container sees it; O_EXCL acquisition makes the check atomic
            # across containers, and the lock serializes requests in this container.
            active = self.read_active(fresh=True)
            active_id = active.get("job_id") if isinstance(active, dict) else None
            if active_id:
                active_job = self.get_job(active_id)
                if active_job is not None and active_job.get("status") in ACTIVE_STATUSES:
                    if not _is_stale(active_job):
                        raise ApiError(
                            409,
                            f"Fine-tuning job {active_id} is {active_job['status']} on this "
                            "provider. Only one job can use the GPU at a time; wait for it to "
                            "finish, cancel it, or retry after it stops.",
                            code="job_running",
                        )
                    # The container that owned this job is gone (the marker has not been
                    # touched for ACTIVE_STALE_SECONDS). Record the loss rather than
                    # leaving a job that says "running" forever.
                    self.finish_job(
                        active_id,
                        "failed",
                        error={
                            "code": "container_stopped",
                            "message": (
                                "The container running this job stopped before the job "
                                "finished. Submit the job again."
                            ),
                            "param": None,
                        },
                    )
                self.clear_active(active_id)

            self._ensure_job_dir(job_id)
            self._write_json(self.job_dir(job_id) / "job.json", job)
            self.append_event(
                job_id,
                "info",
                f"Created job {job_id} for {training_file} (base model {model}).",
                {
                    "model": model,
                    "training_file": training_file,
                    "hyperparameters": dict(hyperparameters),
                },
            )

            acquired = self.try_acquire_active(job_id)
            if not acquired:
                # Another container won the race between the check above and our acquire.
                # If its marker is already stale (or points at nothing), reclaim it once.
                current = self.read_active(fresh=True)
                current_id = current.get("job_id") if isinstance(current, dict) else None
                current_job = self.get_job(current_id) if current_id else None
                if current_job is None or (
                    current_job.get("status") not in ACTIVE_STATUSES
                    or _is_stale(current_job)
                ):
                    if current_job is not None and current_job.get("status") in ACTIVE_STATUSES:
                        self.finish_job(
                            current_id,
                            "failed",
                            error={
                                "code": "container_stopped",
                                "message": (
                                    "The container running this job stopped before the job "
                                    "finished. Submit the job again."
                                ),
                                "param": None,
                            },
                        )
                    self.clear_active(current_id)
                    acquired = self.try_acquire_active(job_id)
            if not acquired:
                self._discard_job(job_id)
                raise ApiError(
                    409,
                    "Another fine-tuning job started at the same time on this provider. "
                    "Only one job can use the GPU at a time; wait for it to finish, cancel "
                    "it, or retry after it stops.",
                    code="job_running",
                )
            self.commit()
        return job

    def cancel_job(self, job_id: str) -> dict[str, Any]:
        job = self.get_job(job_id)
        if job is None:
            raise ApiError(404, f"No fine-tuning job with id {job_id!r} exists.", code="job_not_found")
        status = job.get("status")
        if status in TERMINAL_STATUSES:
            raise ApiError(
                400,
                f"Fine-tuning job {job_id} is already {status}; it cannot be cancelled.",
                code="job_not_cancellable",
            )

        self.write_control(job_id, {"cancel": True})
        if status in ("validating_files", "queued"):
            # No optimizer step has run, so there is nothing to wait for.
            return self.finish_job(job_id, "cancelled")
        self.append_event(
            job_id,
            "info",
            "Cancellation requested; the trainer will stop after the current step.",
            {"status": status},
        )
        return self.get_job(job_id) or job

    def pause_job(self, job_id: str) -> dict[str, Any]:
        job = self.get_job(job_id)
        if job is None:
            raise ApiError(404, f"No fine-tuning job with id {job_id!r} exists.", code="job_not_found")
        if job.get("status") != "running":
            raise ApiError(
                400,
                f"Fine-tuning job {job_id} is {job.get('status')}; only a running job can be paused.",
                code="job_not_pausable",
            )
        control = self.read_control(job_id)
        if control["cancel"]:
            raise ApiError(
                400,
                f"Fine-tuning job {job_id} is already being cancelled.",
                code="job_not_pausable",
            )
        if control["pause"]:
            raise ApiError(
                400,
                f"Fine-tuning job {job_id} is already paused.",
                code="job_already_paused",
            )
        self.write_control(job_id, {"pause": True})
        self.append_event(
            job_id,
            "info",
            "Pause requested; the trainer will pause between steps.",
            {},
        )
        return self.get_job(job_id) or job

    def resume_job(self, job_id: str) -> dict[str, Any]:
        job = self.get_job(job_id)
        if job is None:
            raise ApiError(404, f"No fine-tuning job with id {job_id!r} exists.", code="job_not_found")
        if job.get("status") != "running":
            raise ApiError(
                400,
                f"Fine-tuning job {job_id} is {job.get('status')}; only a running job can be resumed.",
                code="job_not_resumable",
            )
        control = self.read_control(job_id)
        if control["cancel"]:
            raise ApiError(
                400,
                f"Fine-tuning job {job_id} is being cancelled and cannot be resumed.",
                code="job_not_resumable",
            )
        if not control["pause"]:
            raise ApiError(
                400,
                f"Fine-tuning job {job_id} is not paused.",
                code="job_not_paused",
            )
        self.write_control(job_id, {"pause": False})
        self.append_event(job_id, "info", "Resume requested; training continues from the next step.", {})
        return self.get_job(job_id) or job

    def reconcile_orphans(self, threshold: float = ACTIVE_STALE_SECONDS) -> None:
        """Fail jobs left "running" by a container that died, at container startup.

        The marker is authoritative: a job that is active but whose job.json has not been
        touched for ``threshold`` seconds has no trainer behind it. A live trainer touches
        the job once per step.
        """
        active = self.read_active(fresh=True)
        active_id = active.get("job_id") if isinstance(active, dict) else None
        for job in self._all_jobs():
            if job.get("status") not in ACTIVE_STATUSES:
                continue
            if job.get("id") == active_id:
                if not _is_stale(job, threshold):
                    continue
            elif not _is_stale(job, threshold):
                continue
            self.finish_job(
                job["id"],
                "failed",
                error={
                    "code": "container_stopped",
                    "message": "The container running this job stopped before the job finished. Submit the job again.",
                    "param": None,
                },
            )
        if active_id:
            job = self.get_job(active_id)
            if job is None or job.get("status") in TERMINAL_STATUSES:
                self.clear_active(active_id)


STORE = Store(STATE_DIR, commit=None, reload=None)

# --------------------------------------------------------------------------------------
# FastAPI application
# --------------------------------------------------------------------------------------

asgi_app = FastAPI(title="Manarouter fine-tuning reference", version="0.1.0")


@asgi_app.exception_handler(ApiError)
def _handle_api_error(request: Request, exc: ApiError) -> JSONResponse:
    return _error_response(exc.status_code, exc)


@asgi_app.exception_handler(HTTPException)
def _handle_http_error(request: Request, exc: HTTPException) -> JSONResponse:
    detail = exc.detail
    if isinstance(detail, dict) and "error" in detail:
        return JSONResponse(status_code=exc.status_code, content=detail)
    return JSONResponse(
        status_code=exc.status_code,
        content={
            "error": {
                "message": str(detail),
                "type": "invalid_request_error",
                "param": None,
                "code": None,
            }
        },
    )


# -- files ------------------------------------------------------------------------------


@asgi_app.post("/v1/files")
async def create_file(
    file: UploadFile = File(...),
    purpose: str = Form("fine-tune"),
) -> dict[str, Any]:
    """Upload a training file. The body is multipart/form-data with a ``file`` part."""
    data = await file.read()
    if isinstance(data, str):
        data = data.encode("utf-8")
    if not data:
        raise ApiError(400, "The uploaded file is empty.", code="invalid_file", param="file")
    filename = os.path.basename(str(file.filename or "").replace("\\", "/")) or "training.jsonl"
    purpose = (purpose or "fine-tune").strip() or "fine-tune"
    return STORE.create_file(filename, purpose, data)


@asgi_app.get("/v1/files")
def list_files(limit: int = DEFAULT_LIMIT, after: str | None = None) -> dict[str, Any]:
    page, has_more = _page(STORE.list_files(), limit, after)
    return {"object": "list", "data": page, "has_more": has_more}


@asgi_app.get("/v1/files/{file_id}/content")
def get_file_content(file_id: str) -> Response:
    meta = STORE.get_file(file_id)
    if meta is None:
        raise ApiError(404, f"No file with id {file_id!r} exists.", code="file_not_found", param="file_id")
    data = STORE.file_content(file_id)
    if data is None:
        raise ApiError(404, f"The content of file {file_id!r} is missing.", code="file_not_found", param="file_id")
    filename = str(meta.get("filename") or "file.jsonl").replace('"', "").replace("\n", " ")
    return Response(
        content=data,
        media_type="application/jsonl",
        headers={"Content-Disposition": f'attachment; filename="{filename}"'},
    )


@asgi_app.get("/v1/files/{file_id}")
def get_file(file_id: str) -> dict[str, Any]:
    meta = STORE.get_file(file_id)
    if meta is None:
        raise ApiError(404, f"No file with id {file_id!r} exists.", code="file_not_found", param="file_id")
    return meta


# -- fine-tuning jobs -------------------------------------------------------------------


@asgi_app.post("/v1/fine_tuning/jobs")
def create_fine_tuning_job(payload: dict[str, Any]) -> dict[str, Any]:
    """Create a job and return immediately with ``status: "validating_files"``.

    Validation and training happen in a background thread; the HTTP response does not wait
    for either. A bad training file therefore produces a ``failed`` job, not a 500.
    """
    global _ACTIVE_JOB

    job = STORE.create_job(payload)
    with _ACTIVE_LOCK:
        _ACTIVE_JOB = job["id"]
    thread = threading.Thread(
        target=_run_job,
        args=(job["id"],),
        name=f"finetune-{job['id']}",
        daemon=True,
    )
    try:
        thread.start()
    except Exception as exc:  # pragma: no cover - thread start failure is exotic
        logger.exception("could not start the training thread")
        STORE.finish_job(
            job["id"],
            "failed",
            error={
                "code": "trainer_start_failed",
                "message": f"The training thread could not be started: {exc}",
                "param": None,
            },
        )
        with _ACTIVE_LOCK:
            _ACTIVE_JOB = None
        raise ApiError(
            500,
            f"The training thread could not be started: {exc}",
            code="trainer_start_failed",
            error_type="server_error",
        ) from None
    # Return the object as created. A fast validation can already have moved the stored job
    # to queued/running by now, but OpenAI clients expect this response to say
    # ``validating_files``; later status is observed through GET.
    return public_job(job)


@asgi_app.get("/v1/fine_tuning/jobs")
def list_fine_tuning_jobs(limit: int = DEFAULT_LIMIT, after: str | None = None) -> dict[str, Any]:
    page, has_more = _page(STORE.list_jobs(), limit, after)
    return {"object": "list", "data": [public_job(job) for job in page], "has_more": has_more}


@asgi_app.get("/v1/fine_tuning/jobs/{job_id}")
def get_fine_tuning_job(job_id: str) -> dict[str, Any]:
    job = STORE.get_job(job_id)
    if job is None:
        raise ApiError(404, f"No fine-tuning job with id {job_id!r} exists.", code="job_not_found")
    return public_job(job)


@asgi_app.post("/v1/fine_tuning/jobs/{job_id}/cancel")
def cancel_fine_tuning_job(job_id: str) -> dict[str, Any]:
    return public_job(STORE.cancel_job(job_id))


@asgi_app.post("/v1/fine_tuning/jobs/{job_id}/pause")
def pause_fine_tuning_job(job_id: str) -> dict[str, Any]:
    return public_job(STORE.pause_job(job_id))


@asgi_app.post("/v1/fine_tuning/jobs/{job_id}/resume")
def resume_fine_tuning_job(job_id: str) -> dict[str, Any]:
    return public_job(STORE.resume_job(job_id))


@asgi_app.get("/v1/fine_tuning/jobs/{job_id}/events")
def list_fine_tuning_events(
    job_id: str, limit: int = DEFAULT_LIMIT, after: str | None = None
) -> dict[str, Any]:
    if STORE.get_job(job_id) is None:
        raise ApiError(404, f"No fine-tuning job with id {job_id!r} exists.", code="job_not_found")
    page, has_more = _page(STORE.list_events(job_id), limit, after)
    return {"object": "list", "data": page, "has_more": has_more}


@asgi_app.get("/v1/fine_tuning/jobs/{job_id}/checkpoints")
def list_fine_tuning_checkpoints(
    job_id: str, limit: int = DEFAULT_LIMIT, after: str | None = None
) -> dict[str, Any]:
    if STORE.get_job(job_id) is None:
        raise ApiError(404, f"No fine-tuning job with id {job_id!r} exists.", code="job_not_found")
    page, has_more = _page(STORE.list_checkpoints(job_id), limit, after)
    return {"object": "list", "data": page, "has_more": has_more}


# -- inference --------------------------------------------------------------------------


@asgi_app.get("/v1/models")
def list_models() -> dict[str, Any]:
    """The base model plus every succeeded, non-deleted fine-tune."""
    data: list[dict[str, Any]] = [
        {"id": BASE_MODEL, "object": "model", "created": 0, "owned_by": "manarouter"}
    ]
    seen = {BASE_MODEL}
    for job in sorted(STORE._all_jobs(), key=lambda item: item.get("created_at", 0), reverse=True):
        if job.get("status") != "succeeded" or job.get("_deleted"):
            continue
        name = job.get("fine_tuned_model")
        if not isinstance(name, str) or not name or name in seen:
            continue
        seen.add(name)
        data.append(
            {
                "id": name,
                "object": "model",
                "created": int(job.get("finished_at") or job.get("created_at") or 0),
                "owned_by": "manarouter",
            }
        )
    return {"object": "list", "data": data}


@asgi_app.delete("/v1/models/{model:path}")
def delete_model(model: str) -> dict[str, Any]:
    """Hide a fine-tuned model from the listing; the adapter is kept for audit."""
    model = model.strip()
    job = _find_succeeded_job(model)
    if job is None:
        if model == BASE_MODEL:
            raise ApiError(
                400,
                "The base model cannot be deleted.",
                code="model_not_deletable",
                param="model",
            )
        raise ApiError(404, f"The model {model!r} does not exist.", code="model_not_found", param="model")
    STORE.update_job(job["id"], _deleted=True)
    return {"id": model, "object": "model", "deleted": True}


@asgi_app.get("/healthz")
def healthz() -> dict[str, Any]:
    """Liveness only: no volume commit, no GPU touch, no model load."""
    return {
        "status": "ok",
        "service": APP_NAME,
        "base_model": BASE_MODEL,
        "gpu": GPU,
        "active_job": _ACTIVE_JOB,
    }


# --------------------------------------------------------------------------------------
# Training
# --------------------------------------------------------------------------------------

_ACTIVE_JOB: str | None = None
_ACTIVE_LOCK = threading.Lock()

_MODEL_CACHE: dict[str, Any] = {}
_MODEL_LOCK = threading.RLock()


def _find_succeeded_job(model_name: str) -> dict[str, Any] | None:
    for job in STORE._all_jobs():
        if (
            job.get("status") == "succeeded"
            and not job.get("_deleted")
            and job.get("fine_tuned_model") == model_name
        ):
            return job
    return None


def _clear_model_cache() -> None:
    """Drop the cached model and return its GPU memory to the allocator, best effort."""
    if not _MODEL_CACHE:
        return
    _MODEL_CACHE.clear()
    gc.collect()
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.empty_cache()
    except Exception:
        pass


def _evict_models() -> None:
    with _MODEL_LOCK:
        _clear_model_cache()


def _active_job_id() -> str | None:
    """The job currently holding the GPU, from memory or the cross-container marker."""
    if _ACTIVE_JOB:
        return _ACTIVE_JOB
    active = STORE.read_active(fresh=True)
    job_id = active.get("job_id") if isinstance(active, dict) else None
    if not isinstance(job_id, str):
        return None
    job = STORE.get_job(job_id)
    if job is None or job.get("status") in TERMINAL_STATUSES:
        STORE.clear_active(job_id)
        return None
    return job_id


def _get_model(model_name: str):
    """Load the base model, or the base plus a PEFT adapter, and cache one entry.

    One entry only: the card is 96 GB and keeping a base model and an adapter on it while a
    job starts would double the memory use. The cache is cleared at the start of every job
    so training gets the whole card.
    """
    with _MODEL_LOCK:
        cached = _MODEL_CACHE.get(model_name)
        if cached is not None:
            return cached

        job = _find_succeeded_job(model_name)
        adapter_dir: Path | None = None
        if job is None:
            if model_name != BASE_MODEL:
                raise ApiError(
                    404,
                    f"The model {model_name!r} does not exist on this provider. "
                    f"Use {BASE_MODEL!r} or the fine_tuned_model of a succeeded job.",
                    code="model_not_found",
                    param="model",
                )
            base_model = BASE_MODEL
        else:
            base_model = job["model"]
            adapter_dir = STORE.job_dir(job["id"]) / "adapter"
            if not (adapter_dir / "adapter_config.json").exists():
                raise ApiError(
                    404,
                    f"The fine-tuned model {model_name!r} has no adapter on this provider.",
                    code="model_not_found",
                    param="model",
                )

        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer

        _clear_model_cache()
        token = os.environ.get("HF_TOKEN") or None
        tokenizer = AutoTokenizer.from_pretrained(base_model, trust_remote_code=True, token=token)
        if tokenizer.pad_token is None:
            tokenizer.pad_token = tokenizer.eos_token

        has_cuda = torch.cuda.is_available()
        dtype = torch.bfloat16 if has_cuda and torch.cuda.is_bf16_supported() else (
            torch.float16 if has_cuda else torch.float32
        )
        model = AutoModelForCausalLM.from_pretrained(
            base_model,
            dtype=dtype,
            device_map="auto" if has_cuda else None,
            trust_remote_code=True,
            token=token,
        )
        if adapter_dir is not None:
            from peft import PeftModel

            model = PeftModel.from_pretrained(model, str(adapter_dir))
        model.eval()
        _MODEL_CACHE[model_name] = (model, tokenizer, base_model)
        return _MODEL_CACHE[model_name]


@asgi_app.post("/v1/chat/completions")
def chat_completions(payload: dict[str, Any]) -> dict[str, Any]:
    """A minimal non-streaming chat completion against the base model or an adapter."""
    if not isinstance(payload, dict):
        raise ApiError(400, "The request body must be a JSON object.", code="invalid_request")
    model_name = payload.get("model") or BASE_MODEL
    if not isinstance(model_name, str) or not model_name.strip():
        raise ApiError(400, "model must be a non-empty string.", param="model")
    model_name = model_name.strip()

    active = _active_job_id()
    if active:
        raise ApiError(
            409,
            f"Fine-tuning job {active} is using the GPU on this provider; chat is "
            "unavailable until that job finishes or is cancelled.",
            code="job_running",
            error_type="server_error",
        )

    messages = payload.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ApiError(400, "messages must be a non-empty array.", param="messages")
    for index, message in enumerate(messages):
        if not isinstance(message, dict) or not isinstance(message.get("role"), str):
            raise ApiError(
                400,
                f"messages[{index}] must be an object with a string role.",
                param="messages",
            )
        if not isinstance(message.get("content", ""), str):
            raise ApiError(
                400,
                f"messages[{index}].content must be a string.",
                param="messages",
            )

    if payload.get("stream"):
        raise ApiError(
            400,
            "This reference provider does not implement streaming chat completions; "
            "send stream=false or omit stream.",
            code="streaming_not_supported",
            param="stream",
        )

    try:
        max_tokens = int(payload.get("max_tokens") or payload.get("max_completion_tokens") or 256)
    except (TypeError, ValueError):
        raise ApiError(400, "max_tokens must be an integer.", param="max_tokens") from None
    if max_tokens < 1:
        raise ApiError(400, "max_tokens must be at least 1.", param="max_tokens")

    try:
        temperature = float(payload.get("temperature", 0.7))
    except (TypeError, ValueError):
        raise ApiError(400, "temperature must be a number.", param="temperature") from None
    if temperature < 0:
        raise ApiError(400, "temperature must be non-negative.", param="temperature")

    if model_name != BASE_MODEL and _find_succeeded_job(model_name) is None:
        raise ApiError(
            404,
            f"The model {model_name!r} does not exist.",
            code="model_not_found",
            param="model",
        )

    model, tokenizer, base_model = _get_model(model_name)

    import torch

    prompt = tokenizer.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
    inputs = tokenizer(prompt, return_tensors="pt")
    device = next(model.parameters()).device
    inputs = {key: value.to(device) for key, value in inputs.items()}
    generation = {
        "max_new_tokens": max_tokens,
        "do_sample": temperature > 0,
        "pad_token_id": tokenizer.pad_token_id,
    }
    if temperature > 0:
        generation["temperature"] = temperature
    with torch.no_grad():
        output = model.generate(**inputs, **generation)
    generated = output[0][inputs["input_ids"].shape[-1] :]
    text = tokenizer.decode(generated, skip_special_tokens=True)
    prompt_tokens = int(inputs["input_ids"].shape[-1])
    completion_tokens = int(generated.shape[-1])
    return {
        "id": f"chatcmpl-{uuid.uuid4().hex[:24]}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": model_name,
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "length" if completion_tokens >= max_tokens else "stop",
            }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    }


# --------------------------------------------------------------------------------------
# The training thread
# --------------------------------------------------------------------------------------


def _fail_job(job_id: str, code: str, message: str, param: str | None = None) -> None:
    job = STORE.get_job(job_id)
    if job is None or job.get("status") in TERMINAL_STATUSES:
        return
    STORE.finish_job(
        job_id,
        "failed",
        error={"code": code, "message": message, "param": param},
    )


def _run_job(job_id: str) -> None:
    """Validate the file, train, save the adapter, and record a terminal status."""
    global _ACTIVE_JOB

    try:
        job = STORE.get_job(job_id)
        if job is None or job.get("status") in TERMINAL_STATUSES:
            return

        data = STORE.file_content(job["training_file"])
        records, error = validate_jsonl(data)
        if error is not None:
            _fail_job(job_id, "invalid_training_file", error, "training_file")
            return

        job = STORE.get_job(job_id)
        if job is None or job.get("status") in TERMINAL_STATUSES:
            return

        STORE.set_status(job_id, "queued")
        STORE.set_status(job_id, "running")
        STORE.append_event(
            job_id,
            "info",
            f"Training on {len(records)} example(s) for {job['hyperparameters']['n_epochs']} epoch(s).",
            {"examples": len(records)},
        )

        control = STORE.read_control(job_id, fresh=True)
        if control["cancel"]:
            STORE.finish_job(job_id, "cancelled")
            return

        _train(job_id, job, records)
    except Exception as exc:
        logger.exception("fine-tuning job %s crashed", job_id)
        _fail_job(job_id, "training_failed", f"The training run failed: {exc}")
    finally:
        with _ACTIVE_LOCK:
            if _ACTIVE_JOB == job_id:
                _ACTIVE_JOB = None
        try:
            STORE.clear_active(job_id)
        except Exception:
            logger.exception("could not clear the active-job marker for %s", job_id)


def _train(job_id: str, job: dict[str, Any], records: list[dict[str, Any]]) -> None:
    """Run one LoRA fine-tune with trl 1.17's SFTTrainer.

    The callback is the only place pause/cancel is observed, and it runs between optimizer
    steps. That is cooperative: a job that is stuck inside a very long step cannot be
    interrupted until that step ends, and the README says so.
    """
    import torch
    from datasets import Dataset
    from peft import LoraConfig, get_peft_model
    from transformers import AutoModelForCausalLM, AutoTokenizer, TrainerCallback
    from trl import SFTConfig, SFTTrainer

    # Training gets the whole card: drop any chat model first.
    _evict_models()

    hyperparameters = job["hyperparameters"]
    n_epochs = int(hyperparameters["n_epochs"])
    batch_size = int(hyperparameters["batch_size"])
    learning_rate = 2e-4 * float(hyperparameters["learning_rate_multiplier"])
    seed = int(job["seed"]) if job.get("seed") is not None else 42
    base_model = job["model"]
    token = os.environ.get("HF_TOKEN") or None

    tokenizer = AutoTokenizer.from_pretrained(base_model, trust_remote_code=True, token=token)
    if tokenizer.pad_token is None:
        tokenizer.pad_token = tokenizer.eos_token

    texts = [text for text in (render_example(tokenizer, record) for record in records) if text]
    if not texts:
        _fail_job(
            job_id,
            "invalid_training_file",
            "No training example could be rendered with the base model's chat template.",
            "training_file",
        )
        return

    has_cuda = torch.cuda.is_available()
    dtype = torch.bfloat16 if has_cuda and torch.cuda.is_bf16_supported() else (
        torch.float16 if has_cuda else torch.float32
    )
    model = AutoModelForCausalLM.from_pretrained(
        base_model,
        dtype=dtype,
        device_map="auto" if has_cuda else None,
        trust_remote_code=True,
        token=token,
    )
    if hasattr(model, "config"):
        model.config.use_cache = False

    named_modules = list(model.named_modules())
    linear_leaves: list[str] = []
    for name, module in named_modules:
        if isinstance(module, torch.nn.Linear):
            candidate = name.rsplit(".", 1)[-1]
            # Adapting the output head is not what "first linear layers" means here;
            # skip it so an architecture without q/k/v/o still gets a useful adapter.
            if candidate not in ("lm_head", "embed_out", "output") and candidate not in linear_leaves:
                linear_leaves.append(candidate)
    target_modules = choose_target_modules(
        [name for name, _ in named_modules], linear_leaves
    )
    lora = LoraConfig(
        r=16,
        lora_alpha=32,
        lora_dropout=0.05,
        bias="none",
        task_type="CAUSAL_LM",
        target_modules=target_modules,
    )
    tokenizer.padding_side = "right"
    torch.manual_seed(seed)
    peft_model = get_peft_model(model, lora)
    if has_cuda and hasattr(peft_model, "enable_input_require_grads"):
        # Gradient checkpointing with a frozen base model needs inputs that require grad,
        # otherwise the checkpointed blocks have nothing to backpropagate into.
        peft_model.enable_input_require_grads()
    trainable = sum(p.numel() for p in peft_model.parameters() if p.requires_grad)

    dataset = Dataset.from_dict({"text": texts})
    STORE.append_event(
        job_id,
        "info",
        f"Loaded {base_model} with a LoRA adapter ({trainable:,} trainable parameters) "
        f"targeting {', '.join(target_modules)}.",
        {"trainable_parameters": trainable, "target_modules": target_modules},
    )

    class JobControl(TrainerCallback):
        """Observe pause/cancel between steps; log one metrics event per logged step."""

        def __init__(self) -> None:
            self.paused = False
            self.cancelled = False

        def on_step_end(self, args, state, control, **kwargs):
            try:
                STORE.touch_job(job_id)
                current = STORE.read_control(job_id, fresh=True)
            except Exception:
                logger.exception("could not read control.json for %s", job_id)
                return control
            if current["cancel"]:
                if not self.cancelled:
                    self.cancelled = True
                    try:
                        STORE.append_event(
                            job_id,
                            "info",
                            "Cancellation picked up; stopping after this step.",
                            {"step": int(state.global_step)},
                        )
                    except Exception:
                        logger.exception("could not record cancellation for %s", job_id)
                control.should_training_stop = True
                control.should_save = False
                return control
            ticks = 0
            while current["pause"]:
                if not self.paused:
                    self.paused = True
                    try:
                        STORE.append_event(
                            job_id,
                            "info",
                            "Paused between steps; no optimizer steps will run until resumed.",
                            {"step": int(state.global_step)},
                        )
                    except Exception:
                        logger.exception("could not record pause for %s", job_id)
                time.sleep(1.0)
                ticks += 1
                if ticks % 30 == 0:
                    # Keep the job looking alive while paused, otherwise a request served
                    # by another container would treat it as a dead container's job and
                    # fail it after ACTIVE_STALE_SECONDS.
                    try:
                        STORE.touch_job(job_id)
                    except Exception:
                        logger.exception("could not refresh paused job %s", job_id)
                try:
                    current = STORE.read_control(job_id, fresh=True)
                except Exception:
                    logger.exception("could not re-read control.json for %s", job_id)
                    continue
                if current["cancel"]:
                    self.cancelled = True
                    control.should_training_stop = True
                    control.should_save = False
                    return control
            if self.paused:
                self.paused = False
                try:
                    STORE.append_event(
                        job_id,
                        "info",
                        "Resumed; training continues from the next step.",
                        {"step": int(state.global_step)},
                    )
                except Exception:
                    logger.exception("could not record resume for %s", job_id)
            return control

        def on_log(self, args, state, control, logs=None, **kwargs):
            if not logs or "loss" not in logs:
                return control
            try:
                step = int(state.global_step)
                loss = float(logs["loss"])
                lr = float(logs.get("learning_rate") or 0.0)
                STORE.append_event(
                    job_id,
                    "info",
                    f"Step {step}: train_loss={loss:.4f}",
                    {"step": step, "train_loss": loss, "learning_rate": lr},
                    event_type="metrics",
                )
                STORE.update_job(job_id, _last_step=step, _last_loss=loss)
            except Exception:
                logger.exception("could not record metrics for %s", job_id)
            return control

        def on_save(self, args, state, control, **kwargs):
            try:
                step = int(state.global_step)
                current = STORE.get_job(job_id)
                if current is None:
                    return control
                checkpoint = make_checkpoint(
                    current, step, train_loss=current.get("_last_loss")
                )
                if checkpoint is None:
                    return control
                STORE.add_checkpoint(job_id, checkpoint)
                STORE.update_job(job_id, _last_step=step)
                STORE.append_event(
                    job_id,
                    "info",
                    f"Saved checkpoint {checkpoint['id']} at step {step}.",
                    {"checkpoint": checkpoint["id"], "step": step},
                )
            except Exception:
                logger.exception("could not record a checkpoint for %s", job_id)
            return control

        def on_epoch_end(self, args, state, control, **kwargs):
            try:
                STORE.append_event(
                    job_id,
                    "info",
                    f"Epoch {int(state.epoch or 0)} finished.",
                    {"epoch": int(state.epoch or 0), "step": int(state.global_step)},
                )
            except Exception:
                logger.exception("could not record an epoch for %s", job_id)
            return control

    training = SFTConfig(
        output_dir=str(STORE.job_dir(job_id) / "trainer"),
        max_length=MAX_SEQ_LENGTH,
        dataset_text_field="text",
        packing=False,
        per_device_train_batch_size=batch_size,
        gradient_accumulation_steps=1,
        num_train_epochs=n_epochs,
        learning_rate=learning_rate,
        lr_scheduler_type="constant",
        warmup_steps=0,
        logging_steps=1,
        save_strategy="epoch",
        save_total_limit=2,
        bf16=has_cuda and torch.cuda.is_bf16_supported(),
        fp16=has_cuda and not torch.cuda.is_bf16_supported(),
        gradient_checkpointing=has_cuda,
        report_to="none",
        seed=seed,
        data_seed=seed,
        include_num_input_tokens_seen="non_padding",
        remove_unused_columns=False,
    )

    job_control = JobControl()
    trainer = SFTTrainer(
        model=peft_model,
        args=training,
        train_dataset=dataset,
        processing_class=tokenizer,
        callbacks=[job_control],
    )

    try:
        trainer.train()
    finally:
        try:
            trainer.accelerator.free_memory()
        except Exception:
            pass

    current = STORE.get_job(job_id)
    if current is None or current.get("status") in TERMINAL_STATUSES:
        return
    control = STORE.read_control(job_id, fresh=True)
    if job_control.cancelled or control["cancel"]:
        STORE.finish_job(job_id, "cancelled")
        return

    adapter_dir = STORE.job_dir(job_id) / "adapter"
    tmp_dir = STORE.job_dir(job_id) / "adapter.tmp"
    if tmp_dir.exists():
        shutil.rmtree(tmp_dir, ignore_errors=True)
    try:
        trainer.model.save_pretrained(str(tmp_dir))
        tokenizer.save_pretrained(str(tmp_dir))
        if adapter_dir.exists():
            shutil.rmtree(adapter_dir, ignore_errors=True)
        os.rename(tmp_dir, adapter_dir)
        STORE.commit()
    except Exception as exc:
        _fail_job(job_id, "adapter_save_failed", f"The adapter could not be saved: {exc}")
        return

    trained_tokens = int(getattr(trainer.state, "num_input_tokens_seen", 0) or 0)
    if trained_tokens <= 0:
        # ``include_num_input_tokens_seen="non_padding"`` needs an attention mask; if a
        # trl version stops propagating it, count the prepared (truncated) examples.
        try:
            trained_tokens = int(sum(len(ids) for ids in trainer.train_dataset["input_ids"]))
        except Exception:
            trained_tokens = 0
    STORE.append_event(
        job_id,
        "info",
        f"Fine-tuning succeeded; adapter saved as {job['_fine_tuned_model']}.",
        {
            "fine_tuned_model": job["_fine_tuned_model"],
            "trained_tokens": trained_tokens,
        },
    )
    STORE.finish_job(job_id, "succeeded", trained_tokens=trained_tokens)


# --------------------------------------------------------------------------------------
# Modal plumbing
# --------------------------------------------------------------------------------------

image = (
    modal.Image.debian_slim(python_version="3.12")
    .apt_install("git")
    # RTX-PRO-6000 is Blackwell (sm_120), which needs the CUDA 12.8 PyTorch build. The
    # PyTorch index is not a full PyPI mirror, so PyPI is added for torch's own deps
    # (sympy, networkx, ...). ``+cu128`` pins the CUDA wheel rather than a PyPI build.
    .run_commands(
        "pip install --no-cache-dir 'torch==2.8.0+cu128' "
        "--index-url https://download.pytorch.org/whl/cu128 "
        "--extra-index-url https://pypi.org/simple"
    )
    .pip_install(
        "transformers==5.22.0",
        "peft==0.21.2",
        "trl==1.17.1",
        "accelerate>=1.11.0",
        "datasets>=4.7.0",
        "fastapi>=0.115.0",
        "uvicorn[standard]>=0.30.0",
        "python-multipart>=0.0.9",
        "sentencepiece",
        "protobuf",
    )
    .env(
        {
            "HF_HOME": "/state/hf",
            "TOKENIZERS_PARALLELISM": "false",
            "PYTHONUNBUFFERED": "1",
        }
    )
)

volume = modal.Volume.from_name("manarouter-finetune-state", create_if_missing=True)

app = modal.App(APP_NAME)


@app.cls(
    image=image,
    gpu="RTX-PRO-6000",
    # Modal's default request is 0.125 CPU cores and 128 MiB of RAM, which is not enough
    # to load a model, tokenize a dataset, or run the Python trainer. Request explicit
    # resources so the host side does not OOM before the GPU is the bottleneck.
    cpu=8,
    memory=32768,
    volumes={STATE_DIR: volume},
    timeout=6 * 60 * 60,
    scaledown_window=SCALEDOWN_WINDOW,
)
@modal.concurrent(max_inputs=1)
class FineTuneService:
    """The one service class: a FastAPI app plus one background training slot."""

    @modal.web_server(
        port=8000,
        startup_timeout=1800,
        requires_proxy_auth=True,
    )
    def serve(self) -> None:
        """Start uvicorn in a daemon thread and return, so the web server owns the container.

        This is the class analogue of the spawn-and-return pattern used by Modal's vLLM
        examples: the function that Modal invoked returns as soon as the server is
        listening, and the long-lived server keeps the container alive.
        """
        global _SERVER

        with _SERVER_LOCK:
            if _SERVER is not None and _SERVER.started:
                return
            STORE.bind(volume.commit, volume.reload)
            try:
                STORE.reconcile_orphans()
            except Exception:
                logger.exception("could not reconcile orphaned jobs at startup")

            import uvicorn

            config = uvicorn.Config(
                asgi_app,
                host="0.0.0.0",
                port=8000,
                log_level="info",
                access_log=True,
            )
            server = uvicorn.Server(config)
            thread = threading.Thread(target=server.run, name="uvicorn", daemon=True)
            thread.start()

            deadline = time.time() + 60
            while not server.started:
                if not thread.is_alive():
                    raise RuntimeError("The HTTP server thread exited before it started.")
                if time.time() > deadline:
                    raise RuntimeError("The HTTP server did not start within 60 seconds.")
                time.sleep(0.05)
            _SERVER = server
            logger.info("%s listening on 0.0.0.0:8000", APP_NAME)


_SERVER = None
_SERVER_LOCK = threading.Lock()
