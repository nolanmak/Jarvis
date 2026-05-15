"""AugmentAgent browser sidecar.

asyncio Unix-domain socket server that fronts a long-running Chromium
attached over CDP (port 9223 by default). Talks to the Rust daemon via
NDJSON frames over `${XDG_RUNTIME_DIR}/augmentagent/browser.sock`.

Wire protocol — see issue #75 §6:

    Request  : {"request_id": "<uuid>", "op": "<name>", "params": {...},
                "timeout_ms": 30000}
    Success  : {"request_id": "...", "ok": true,  "result": {...},
                "elapsed_ms": 412}
    Failure  : {"request_id": "...", "ok": false, "error": {
                  "kind": "AuthRequired" | "CaptchaDetected"
                        | "ChromiumDisconnected" | "Timeout"
                        | "SelectorNotFound" | "Navigation" | "Internal",
                  "message": "...", "page_url": "...",
                  "screenshot_b64": "..." }, "elapsed_ms": 30000}

Ops implemented (per #75 §6 + the wave-A spec):
    navigate, click, type, screenshot, get_text, set_input_files,
    wait_for, evaluate, ping.

Concurrent requests on a single connection are supported: each frame is
dispatched to its own asyncio task so a long `wait_for` doesn't block
a `ping`. Ordering is per-page (Playwright's API is not thread-safe but
we serialize per-page via an asyncio.Lock).
"""

from __future__ import annotations

import asyncio
import base64
import json
import logging
import os
import signal
import sys
import time
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Awaitable, Callable

try:
    from playwright.async_api import (
        Browser,
        BrowserContext,
        Error as PWError,
        Page,
        TimeoutError as PWTimeoutError,
        async_playwright,
    )
except ImportError:  # pragma: no cover — surfaced at startup, not at import-time in tests
    print(
        "playwright not installed. run sidecars/browser/setup.sh first.",
        file=sys.stderr,
    )
    raise


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

CDP_URL = os.environ.get("AUGMENTAGENT_BROWSER_CDP", "http://127.0.0.1:9223")

_DEFAULT_RUNTIME = f"/run/user/{os.getuid()}"
_RUNTIME = os.environ.get("XDG_RUNTIME_DIR", _DEFAULT_RUNTIME)
SOCK_PATH = os.environ.get(
    "AUGMENTAGENT_BROWSER_SOCK",
    str(Path(_RUNTIME) / "augmentagent" / "browser.sock"),
)

# Heuristics for AuthRequired / CaptchaDetected — see #75 §9.
_LOGIN_URL_FRAGMENTS = ("/login", "/signin", "/sign_in", "/accounts/login")
_CAPTCHA_SELECTORS = (
    'iframe[src*="recaptcha"]',
    'iframe[src*="hcaptcha"]',
    'iframe[src*="captcha"]',
    'div[id*="captcha"]',
)


logging.basicConfig(
    level=os.environ.get("AUGMENTAGENT_BROWSER_LOG", "INFO"),
    format="%(asctime)s %(levelname)s sidecar %(message)s",
    stream=sys.stderr,
)
log = logging.getLogger("augmentagent.browser.sidecar")


# ---------------------------------------------------------------------------
# Typed errors — round-tripped to the Rust client as `error.kind` strings
# ---------------------------------------------------------------------------


class SidecarError(Exception):
    kind: str = "Internal"


class AuthRequired(SidecarError):
    kind = "AuthRequired"


class CaptchaDetected(SidecarError):
    kind = "CaptchaDetected"


class ChromiumDisconnected(SidecarError):
    kind = "ChromiumDisconnected"


class SelectorNotFound(SidecarError):
    kind = "SelectorNotFound"


class NavigationError(SidecarError):
    kind = "Navigation"


class TimeoutErr(SidecarError):
    kind = "Timeout"


# ---------------------------------------------------------------------------
# Browser pool — single shared Chromium connection, lazy-attached.
# ---------------------------------------------------------------------------


@dataclass
class _Pool:
    pw: Any | None = None
    browser: Browser | None = None
    context: BrowserContext | None = None
    page: Page | None = None
    lock: asyncio.Lock | None = None


_pool = _Pool()


async def _ensure_browser() -> tuple[BrowserContext, Page]:
    """Lazily attach to the running Chromium over CDP. Reattach on disconnect."""
    if _pool.lock is None:
        _pool.lock = asyncio.Lock()

    async with _pool.lock:
        if _pool.browser is not None and _pool.browser.is_connected():
            assert _pool.context is not None and _pool.page is not None
            return _pool.context, _pool.page

        log.info("attaching to chromium at %s", CDP_URL)
        try:
            if _pool.pw is None:
                _pool.pw = await async_playwright().start()
            _pool.browser = await _pool.pw.chromium.connect_over_cdp(CDP_URL)
        except Exception as e:
            raise ChromiumDisconnected(f"connect_over_cdp({CDP_URL}) failed: {e}") from e

        # The default context owns the persistent profile that systemd
        # launched chromium with via --user-data-dir. Don't make a new one.
        contexts = _pool.browser.contexts
        if not contexts:
            raise ChromiumDisconnected("attached chromium has no contexts")
        ctx = contexts[0]

        pages = ctx.pages
        page = pages[0] if pages else await ctx.new_page()
        _pool.context = ctx
        _pool.page = page

        # Hook to mark disconnected so the next call reattaches.
        def _on_disconnect(_b: Browser) -> None:
            log.warning("chromium disconnected")
            _pool.browser = None
            _pool.context = None
            _pool.page = None

        _pool.browser.on("disconnected", _on_disconnect)
        return ctx, page


# ---------------------------------------------------------------------------
# Auth / captcha detection helpers
# ---------------------------------------------------------------------------


async def _detect_auth_or_captcha(page: Page) -> None:
    """Raise AuthRequired or CaptchaDetected if the current page looks gated."""
    url = page.url or ""
    if any(frag in url for frag in _LOGIN_URL_FRAGMENTS):
        raise AuthRequired(f"redirected to login: {url}")
    for sel in _CAPTCHA_SELECTORS:
        try:
            handle = await page.query_selector(sel)
        except PWError:
            continue
        if handle is not None:
            raise CaptchaDetected(f"captcha element matched {sel}")


async def _screenshot_b64(page: Page, full_page: bool = False) -> str:
    png = await page.screenshot(full_page=full_page)
    return base64.b64encode(png).decode("ascii")


# ---------------------------------------------------------------------------
# Op dispatch table
# ---------------------------------------------------------------------------


async def op_ping(_page: Page, _params: dict) -> dict:
    return {"pong": True, "ts": time.time()}


async def op_navigate(page: Page, params: dict) -> dict:
    url = params.get("url")
    if not url:
        raise SidecarError("navigate: 'url' required")
    wait_until = params.get("wait_until", "domcontentloaded")
    try:
        resp = await page.goto(url, wait_until=wait_until)
    except PWTimeoutError as e:
        raise TimeoutErr(f"navigate timeout: {e}") from e
    except PWError as e:
        raise NavigationError(str(e)) from e
    await _detect_auth_or_captcha(page)
    return {
        "url": page.url,
        "status": resp.status if resp is not None else None,
    }


async def op_click(page: Page, params: dict) -> dict:
    sel = params.get("selector")
    if not sel:
        raise SidecarError("click: 'selector' required")
    try:
        await page.click(sel, timeout=params.get("timeout_ms", 10_000))
    except PWTimeoutError as e:
        raise SelectorNotFound(f"click timeout for {sel}: {e}") from e
    return {"selector": sel}


async def op_type(page: Page, params: dict) -> dict:
    sel = params.get("selector")
    text = params.get("text", "")
    submit = bool(params.get("submit", False))
    if not sel:
        raise SidecarError("type: 'selector' required")
    try:
        await page.fill(sel, text, timeout=params.get("timeout_ms", 10_000))
    except PWTimeoutError as e:
        raise SelectorNotFound(f"type timeout for {sel}: {e}") from e
    if submit:
        await page.press(sel, "Enter")
    return {"selector": sel, "len": len(text), "submitted": submit}


async def op_screenshot(page: Page, params: dict) -> dict:
    sel = params.get("selector")
    full_page = bool(params.get("full_page", False))
    save_path = params.get("path")  # optional server-side save (e.g. /tmp/...)

    try:
        if sel:
            handle = await page.query_selector(sel)
            if handle is None:
                raise SelectorNotFound(f"screenshot: selector {sel} not found")
            png = await handle.screenshot()
        else:
            png = await page.screenshot(full_page=full_page)
    except PWTimeoutError as e:
        raise TimeoutErr(f"screenshot timeout: {e}") from e

    if save_path:
        Path(save_path).parent.mkdir(parents=True, exist_ok=True)
        Path(save_path).write_bytes(png)

    return {
        "b64": base64.b64encode(png).decode("ascii"),
        "path": save_path,
        "bytes": len(png),
    }


async def op_get_text(page: Page, params: dict) -> dict:
    sel = params.get("selector", "body")
    limit = int(params.get("limit", 16_384))
    try:
        handle = await page.query_selector(sel)
    except PWError as e:
        raise SidecarError(f"get_text: query failed: {e}") from e
    if handle is None:
        raise SelectorNotFound(f"get_text: selector {sel} not found")
    txt = (await handle.inner_text()) or ""
    if len(txt) > limit:
        txt = txt[:limit]
    return {"selector": sel, "text": txt, "len": len(txt)}


async def op_set_input_files(page: Page, params: dict) -> dict:
    sel = params.get("selector")
    paths = params.get("paths") or []
    if not sel:
        raise SidecarError("set_input_files: 'selector' required")
    if not isinstance(paths, list):
        raise SidecarError("set_input_files: 'paths' must be a list")
    for p in paths:
        if not Path(p).exists():
            raise SidecarError(f"set_input_files: file not found: {p}")
    try:
        await page.set_input_files(sel, paths, timeout=params.get("timeout_ms", 10_000))
    except PWTimeoutError as e:
        raise SelectorNotFound(f"set_input_files timeout for {sel}: {e}") from e
    return {"selector": sel, "files": paths}


async def op_wait_for(page: Page, params: dict) -> dict:
    sel = params.get("selector")
    state = params.get("state", "visible")
    timeout_ms = int(params.get("timeout_ms", 15_000))
    if not sel:
        raise SidecarError("wait_for: 'selector' required")
    try:
        await page.wait_for_selector(sel, state=state, timeout=timeout_ms)
    except PWTimeoutError as e:
        raise TimeoutErr(f"wait_for timeout for {sel}: {e}") from e
    return {"selector": sel, "state": state}


async def op_evaluate(page: Page, params: dict) -> dict:
    js = params.get("js") or params.get("expression")
    if not js:
        raise SidecarError("evaluate: 'js' required")
    try:
        result = await page.evaluate(js)
    except PWError as e:
        raise SidecarError(f"evaluate failed: {e}") from e
    # Coerce to JSON-able. evaluate already returns serializable values for
    # primitives + plain objects; non-serializable returns surface as None.
    try:
        json.dumps(result)
    except (TypeError, ValueError):
        result = repr(result)
    return {"value": result}


_OPS: dict[str, Callable[[Page, dict], Awaitable[dict]]] = {
    "ping": op_ping,
    "navigate": op_navigate,
    "click": op_click,
    "type": op_type,
    "screenshot": op_screenshot,
    "get_text": op_get_text,
    "set_input_files": op_set_input_files,
    "wait_for": op_wait_for,
    "evaluate": op_evaluate,
}


# ---------------------------------------------------------------------------
# Per-page lock to serialize Playwright calls on the shared page
# ---------------------------------------------------------------------------

_page_lock = asyncio.Lock()


async def _dispatch(req: dict) -> dict:
    request_id = req.get("request_id", "")
    op = req.get("op", "")
    params = req.get("params") or {}
    timeout_ms = int(req.get("timeout_ms", 30_000))

    handler = _OPS.get(op)
    if handler is None:
        return _err(request_id, "Internal", f"unknown op: {op}", elapsed_ms=0)

    started = time.monotonic()
    try:
        _ctx, page = await _ensure_browser()
    except SidecarError as e:
        return _err(request_id, e.kind, str(e), elapsed_ms=_ms(started))
    except Exception as e:  # noqa: BLE001
        return _err(
            request_id,
            "Internal",
            f"attach failed: {e}",
            elapsed_ms=_ms(started),
        )

    try:
        async with _page_lock:
            result = await asyncio.wait_for(handler(page, params), timeout=timeout_ms / 1000)
        return {
            "request_id": request_id,
            "ok": True,
            "result": result,
            "elapsed_ms": _ms(started),
        }
    except asyncio.TimeoutError:
        return await _err_with_screenshot(
            page, request_id, "Timeout", f"op {op} timed out after {timeout_ms}ms",
            _ms(started),
        )
    except (AuthRequired, CaptchaDetected) as e:
        return await _err_with_screenshot(page, request_id, e.kind, str(e), _ms(started))
    except SidecarError as e:
        return await _err_with_screenshot(page, request_id, e.kind, str(e), _ms(started))
    except PWError as e:
        # Likely Chromium disconnected mid-call.
        msg = str(e)
        kind = "ChromiumDisconnected" if "closed" in msg.lower() or "disconnect" in msg.lower() else "Internal"
        return await _err_with_screenshot(page, request_id, kind, msg, _ms(started))
    except Exception as e:  # noqa: BLE001
        log.error("op %s crashed: %s\n%s", op, e, traceback.format_exc())
        return await _err_with_screenshot(
            page, request_id, "Internal", f"{type(e).__name__}: {e}", _ms(started)
        )


def _ms(started: float) -> int:
    return int((time.monotonic() - started) * 1000)


def _err(request_id: str, kind: str, message: str, elapsed_ms: int) -> dict:
    return {
        "request_id": request_id,
        "ok": False,
        "error": {"kind": kind, "message": message},
        "elapsed_ms": elapsed_ms,
    }


async def _err_with_screenshot(
    page: Page | None,
    request_id: str,
    kind: str,
    message: str,
    elapsed_ms: int,
) -> dict:
    error: dict[str, Any] = {"kind": kind, "message": message}
    if page is not None:
        try:
            error["page_url"] = page.url
            error["screenshot_b64"] = await _screenshot_b64(page, full_page=False)
        except Exception:  # noqa: BLE001
            pass
    return {
        "request_id": request_id,
        "ok": False,
        "error": error,
        "elapsed_ms": elapsed_ms,
    }


# ---------------------------------------------------------------------------
# Server loop — one task per request frame so concurrent ops don't head-of-
# line block each other.
# ---------------------------------------------------------------------------


async def _handle_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    peer = writer.get_extra_info("peername") or "<unix>"
    log.info("client connected: %s", peer)
    write_lock = asyncio.Lock()

    async def _send(resp: dict) -> None:
        line = (json.dumps(resp, separators=(",", ":")) + "\n").encode("utf-8")
        async with write_lock:
            writer.write(line)
            await writer.drain()

    async def _process(line: bytes) -> None:
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            await _send(_err("", "Internal", f"bad json: {e}", 0))
            return
        resp = await _dispatch(req)
        await _send(resp)

    pending: set[asyncio.Task] = set()
    try:
        while True:
            line = await reader.readline()
            if not line:
                break
            task = asyncio.create_task(_process(line))
            pending.add(task)
            task.add_done_callback(pending.discard)
    finally:
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        try:
            writer.close()
            await writer.wait_closed()
        except Exception:  # noqa: BLE001
            pass
        log.info("client disconnected: %s", peer)


async def _serve() -> None:
    sock_path = Path(SOCK_PATH)
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    if sock_path.exists():
        sock_path.unlink()

    server = await asyncio.start_unix_server(_handle_client, path=str(sock_path))
    os.chmod(sock_path, 0o600)
    log.info("listening on %s (CDP=%s)", sock_path, CDP_URL)

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, stop.set)
        except NotImplementedError:
            pass

    async with server:
        serve_task = asyncio.create_task(server.serve_forever())
        await stop.wait()
        log.info("shutdown signal received")
        serve_task.cancel()
        try:
            await serve_task
        except asyncio.CancelledError:
            pass

    # Best-effort cleanup
    try:
        if _pool.browser is not None:
            await _pool.browser.close()
        if _pool.pw is not None:
            await _pool.pw.stop()
    except Exception:  # noqa: BLE001
        pass
    try:
        sock_path.unlink()
    except FileNotFoundError:
        pass


def main() -> int:
    try:
        asyncio.run(_serve())
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
