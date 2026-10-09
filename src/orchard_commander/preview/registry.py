"""Which previewer shows a file: registration, discovery, scoring, fallback."""

from __future__ import annotations

import logging
import threading
from importlib.metadata import entry_points

from orchard_commander import vfs
from orchard_commander.preview.api import (
    CannotPreview,
    Candidate,
    LoadContext,
    NeedsDownload,
    Outcome,
    Previewer,
    Score,
    ShowMessage,
)
from orchard_commander.preview.source import PreviewSource

ENTRY_POINT_GROUP = "orchard_commander.previewers"

log = logging.getLogger(__name__)
_lock = threading.Lock()
_previewers: dict[str, Previewer] = {}
_initialized = False


def register(previewer: Previewer | type[Previewer], *, replace: bool = False) -> None:
    """Add a previewer (class or instance). Previewers whose `available()` is
    False are ignored. A different previewer with the same id replaces the old
    one only with `replace=True`."""
    instance = previewer() if isinstance(previewer, type) else previewer
    if not type(instance).available():
        log.info("previewer %s unavailable (missing dependency?)", instance.id)
        return
    with _lock:
        if instance.id in _previewers and not replace:
            raise ValueError(f"A previewer with id {instance.id!r} is already registered")
        _previewers[instance.id] = instance


def unregister(previewer_id: str) -> None:
    with _lock:
        _previewers.pop(previewer_id, None)


def _ensure_initialized() -> None:
    global _initialized
    with _lock:
        if _initialized:
            return
        _initialized = True
    from orchard_commander.preview import builtin

    for cls in builtin.PREVIEWERS:
        register(cls)
    for ep in entry_points(group=ENTRY_POINT_GROUP):
        try:
            register(ep.load(), replace=True)  # add-ons may override built-ins
        except Exception:  # a broken add-on must not break previews
            log.exception("could not load previewer %s from %s", ep.name, ep.value)


def previewers() -> list[Previewer]:
    _ensure_initialized()
    with _lock:
        return list(_previewers.values())


def get(previewer_id: str) -> Previewer | None:
    _ensure_initialized()
    with _lock:
        return _previewers.get(previewer_id)


def candidates(src: PreviewSource, *, may_read: bool = True) -> list[Candidate]:
    """Previewers that can show `src`, best first.

    Scoring is cheap by name/size first; the file's first bytes are read (once)
    only if a head-sniffing previewer could still win — and only if `may_read`
    (reading an online-only cloud file downloads all of it).
    """
    available = previewers()
    info = src.info()
    scored = _score(available, info)
    best = scored[0].score if scored else Score.NO
    sniffers = [p for p in available if p.needs_head]
    if may_read and sniffers and not info.is_dir and info.size > 0 and best < Score.FORMAT:
        info = src.info(with_head=True)
        scored = _score(available, info)
    return scored


def _score(available: list[Previewer], info) -> list[Candidate]:
    result = []
    for p in available:
        try:
            score = int(p.score(info))
        except Exception:
            log.exception("previewer %s failed to score %s", p.id, info.name)
            continue
        if score > 0:
            result.append(Candidate(p, score))
    result.sort(key=lambda c: c.score, reverse=True)  # stable: registration order breaks ties
    return result


def preview(target: vfs.VPath, ctx: LoadContext, preferred: str | None = None) -> Outcome:
    """Pick and run previewers for `target` (worker thread).

    Tries the preferred previewer (if it can show the file), then the others by
    score. `CannotPreview` falls through to the next; `ShowMessage` and other
    errors end the search and are reported.
    """
    outcome = Outcome()
    try:
        src = PreviewSource(target)
        entry = src.entry
        too_big = (entry.is_online_only and ctx.download_limit is not None
                   and entry.size > ctx.download_limit)
        outcome.candidates = candidates(src, may_read=not too_big)
    except Exception as exc:
        outcome.error = exc
        return outcome
    if too_big:  # don't start a big cloud download just because the cursor passed by
        outcome.error = NeedsDownload(entry.size)
        return outcome
    order = sorted(outcome.candidates, key=lambda c: c.previewer.id != preferred)
    first_failure: BaseException | None = None
    for candidate in order:
        if ctx.is_cancelled():
            return outcome
        try:
            outcome.loaded = candidate.previewer.load(src, ctx)
            outcome.previewer = candidate.previewer
            return outcome
        except CannotPreview as exc:
            first_failure = first_failure or exc
            continue
        except ShowMessage as exc:
            outcome.previewer = candidate.previewer
            outcome.error = exc
            return outcome
        except Exception as exc:
            outcome.previewer = candidate.previewer
            outcome.error = exc
            return outcome
    outcome.error = first_failure or CannotPreview("No previewer can show this file")
    return outcome
