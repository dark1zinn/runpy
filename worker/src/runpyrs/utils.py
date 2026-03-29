"""Typing utilities for the runpyrs package.

Re-exported from the top-level package so users can write::

    from runpyrs import Worker, ExecuteResult, Message

These types improve editor auto-complete, static analysis, and serve as
living documentation for the Runpy wire protocol.

HTTP-like Protocol Schema
=========================

The protocol uses a JSON structure inspired by HTTP:

    {
        "method": "EXECUTE",
        "path": "/task",
        "headers": {
            "X-Worker-Id": "my_worker_01012026-1200_Ax4f",
            "X-Socket-Path": "/tmp/runpy/rp_my_worker.sock",
            "Content-Type": "application/json"
        },
        "body": { "task": "process_data", "input": [...] }
    }

Methods:
    - GET:       Request information (status, health, etc.)
    - POST:      Send data or trigger an action
    - PUT:       Update existing data/state
    - DELETE:    Remove/clear data
    - EXECUTE:   Execute the worker's main business logic
    - RETRY:     Re-execute the last payload
    - TERMINATE: Request graceful termination
    - META:      Send/receive metadata about the worker
    - READY:     Signal the worker is ready
    - STATUS:    Response with status information
    - INFO:      Generic informational message
    - DEBUG:     Debug-level message
    - DONE:      Signal successful completion
    - ERROR:     Error response
    - ACTION:    Perform a named action with parameters
"""

from __future__ import annotations

from typing import Any, Dict, Literal, Optional, TypedDict, Union


# ══════════════════════════════════════════════════════════════════════════════
# METHODS
# ══════════════════════════════════════════════════════════════════════════════

# Standard HTTP-like methods
HttpMethod = Literal["GET", "POST", "PUT", "DELETE"]
"""Standard HTTP-like methods."""

# Custom Runpy methods
RunpyMethod = Literal[
    "EXECUTE", "RETRY", "TERMINATE", "META", "READY", "STATUS", "INFO", "DEBUG", "DONE", "ERROR", "ACTION"
]
"""Custom Runpy protocol methods."""

Method = Union[HttpMethod, RunpyMethod]
"""Any valid method in the Runpy protocol."""

# Legacy aliases for backward compatibility
InternalMessageType = Literal["EXECUTE", "TERMINATE", "META", "RETRY"]
"""Message types handled internally by the Worker base class."""

BuiltinResponseType = Literal["READY", "DONE", "ERROR", "DEBUG"]
"""Response types the Worker emits back to the Rust manager."""

MessageType = Method
"""Alias for Method - any message type that can appear on the wire."""


# ══════════════════════════════════════════════════════════════════════════════
# HEADERS
# ══════════════════════════════════════════════════════════════════════════════

class Headers:
    """Standard header keys used in the protocol."""
    
    # Worker identification
    X_WORKER_ID = "X-Worker-Id"
    X_SOCKET_PATH = "X-Socket-Path"
    
    # Content info
    CONTENT_TYPE = "Content-Type"
    
    # Status/timing
    X_UPTIME = "X-Uptime"
    
    # Action/request metadata
    X_ACTION = "X-Action"
    X_KEY = "X-Key"
    X_STACK_TRACE = "X-Stack-Trace"


HeadersDict = Dict[str, str]
"""Type alias for headers dictionary."""


# ══════════════════════════════════════════════════════════════════════════════
# MESSAGE STRUCTURE
# ══════════════════════════════════════════════════════════════════════════════

class _MessageRequired(TypedDict):
    """Required fields in every Message."""
    method: str


class Message(_MessageRequired, total=False):
    """The unified message structure for all communication.

    Follows an HTTP-like schema:
        {
            "method": "EXECUTE",
            "path": "/execute",
            "headers": {
                "X-Worker-Id": "worker_name",
                "X-Socket-Path": "/tmp/runpy/rp_xxx.sock"
            },
            "body": { ... }
        }
    """
    path: str
    headers: Dict[str, str]
    body: Dict[str, Any]


# Legacy alias
Envelope = Message
"""Legacy alias for Message - the dict received from Rust manager."""


class MetaData(TypedDict, total=False):
    """Contents of ``body`` in a META message."""
    name: str


# ══════════════════════════════════════════════════════════════════════════════
# MESSAGE BUILDERS
# ══════════════════════════════════════════════════════════════════════════════

def create_message(
    method: Method,
    path: str = "",
    headers: Optional[HeadersDict] = None,
    body: Optional[Dict[str, Any]] = None,
) -> Message:
    """Create a new protocol message.

    Args:
        method: The HTTP-like method (GET, POST, EXECUTE, etc.)
        path: Resource path for routing (e.g., "/status", "/execute")
        headers: Optional headers dict for metadata
        body: Optional payload body

    Returns:
        A Message dict ready to be serialized to JSON
    """
    msg: Message = {"method": method}
    if path:
        msg["path"] = path
    if headers:
        msg["headers"] = headers
    if body is not None:
        msg["body"] = body
    return msg


def ready_message(message: str, headers: Optional[HeadersDict] = None) -> Message:
    """Create a READY message."""
    msg = create_message("READY", "/ready", headers, {"message": message})
    return msg


def done_message(
    message: str, data: Dict[str, Any], headers: Optional[HeadersDict] = None
) -> Message:
    """Create a DONE message with result data."""
    return create_message("DONE", "/done", headers, {"message": message, "data": data})


def error_message(
    message: str, stack_trace: Optional[str] = None, headers: Optional[HeadersDict] = None
) -> Message:
    """Create an ERROR message."""
    hdrs = dict(headers) if headers else {}
    if stack_trace:
        hdrs[Headers.X_STACK_TRACE] = stack_trace
    return create_message("ERROR", "/error", hdrs, {"message": message})


def debug_message(
    message: str, data: Dict[str, Any], headers: Optional[HeadersDict] = None
) -> Message:
    """Create a DEBUG message."""
    return create_message("DEBUG", "/debug", headers, {"message": message, "data": data})


def info_message(
    message: str, data: Dict[str, Any], headers: Optional[HeadersDict] = None
) -> Message:
    """Create an INFO message."""
    return create_message("INFO", "/info", headers, {"message": message, "data": data})


def status_response(
    status: str, uptime: int, headers: Optional[HeadersDict] = None
) -> Message:
    """Create a STATUS response."""
    hdrs = dict(headers) if headers else {}
    hdrs[Headers.X_UPTIME] = str(uptime)
    return create_message("STATUS", "/status", hdrs, {"status": status, "uptime": uptime})


# ══════════════════════════════════════════════════════════════════════════════
# USER-FACING TYPE ALIASES
# ══════════════════════════════════════════════════════════════════════════════

ExecutePayload = Dict[str, Any]
"""Type of the *body* dict passed to ``Worker.execute()``."""

ExecuteResult = Optional[Dict[str, Any]]
"""Return type of ``Worker.execute()``.

Return a dict to have it sent back as a ``DONE`` message, or ``None``
for a result-less acknowledgement.
"""

RequestData = Message
"""Alias — the dict handed to ``Worker.handle_request()``."""

HandleRequestResult = None
"""Return type of ``Worker.handle_request()`` (always None)."""

SendData = Optional[Dict[str, Any]]
"""Type accepted for the *body* parameter of ``Worker.send()``."""

# Legacy alias
OutboundMessage = Message
"""Legacy alias for Message - shape of outbound messages."""
