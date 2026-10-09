"""Run blocking work (file systems, network) off the GUI thread.

    run_in_background(fn, on_done, on_error)   fire-and-forget with callbacks
    ask_gui(fn)                                 from a worker: run `fn` on the GUI
                                                thread (e.g. a dialog), wait for it

Callbacks always run on the GUI thread. A callback whose `owner` QObject has
been destroyed is skipped.
"""

from __future__ import annotations

import itertools
import threading
from collections.abc import Callable
from typing import Any

import shiboken6
from PySide6.QtCore import QObject, QThreadPool, Signal, Slot


class _Relay(QObject):
    deliver = Signal(int, bool, object)  # token, ok, result-or-exception
    call = Signal(int)

    def __init__(self) -> None:
        super().__init__()
        self.pending: dict[int, tuple[QObject | None, Callable[[Any], None], Callable[[BaseException], None]]] = {}
        self.calls: dict[int, tuple[Callable[[], Any], list[Any], threading.Event]] = {}
        self.deliver.connect(self._deliver)
        self.call.connect(self._call)

    @Slot(int, bool, object)
    def _deliver(self, token: int, ok: bool, value: object) -> None:
        owner, on_done, on_error = self.pending.pop(token)
        if owner is not None and not shiboken6.isValid(owner):
            return
        if ok:
            on_done(value)
        else:
            assert isinstance(value, BaseException)
            on_error(value)

    @Slot(int)
    def _call(self, token: int) -> None:
        fn, box, done = self.calls.pop(token)
        try:
            box.append((True, fn()))
        except BaseException as exc:  # handed back to the worker
            box.append((False, exc))
        finally:
            done.set()


_relay: _Relay | None = None
_tokens = itertools.count()


def init() -> None:
    """Create the relay on the GUI thread. Call once after QApplication exists."""
    global _relay
    if _relay is None:
        _relay = _Relay()


def _get_relay() -> _Relay:
    if _relay is None:
        if threading.current_thread() is not threading.main_thread():
            raise RuntimeError("tasks.init() must be called on the GUI thread first")
        init()  # first use from the GUI thread (e.g. a previewer view outside the main window)
    assert _relay is not None
    return _relay


def _default_error(exc: BaseException) -> None:
    raise exc


def run_in_background(
    fn: Callable[[], Any],
    on_done: Callable[[Any], None] = lambda _: None,
    on_error: Callable[[BaseException], None] = _default_error,
    *,
    owner: QObject | None = None,
) -> None:
    relay = _get_relay()
    token = next(_tokens)
    relay.pending[token] = (owner, on_done, on_error)

    def job() -> None:
        try:
            result: object = fn()
            ok = True
        except BaseException as exc:
            result, ok = exc, False
        try:
            relay.deliver.emit(token, ok, result)
        except RuntimeError:
            pass  # relay gone: application shutting down

    QThreadPool.globalInstance().start(job)


def ask_gui(fn: Callable[[], Any]) -> Any:
    """Call `fn` on the GUI thread from a worker thread and return its result."""
    relay = _get_relay()
    if threading.current_thread() is threading.main_thread():
        return fn()
    token = next(_tokens)
    box: list[tuple[bool, Any]] = []
    done = threading.Event()
    relay.calls[token] = (fn, box, done)
    relay.call.emit(token)
    done.wait()
    ok, value = box[0]
    if not ok:
        raise value
    return value
