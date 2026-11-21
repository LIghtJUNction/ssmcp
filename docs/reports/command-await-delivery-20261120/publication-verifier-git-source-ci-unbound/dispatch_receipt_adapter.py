"""Parse an operator-captured GitHub dispatch, without executing anything.

The expected hashes and native binding are independent trusted inputs.  Hashes
bind bytes; they do not prove that a command was issued.  Authenticity depends on
the operator's original tool receipt and the reviewer's independent observation.
This module performs no file, process, network, or clock operations.
"""

import base64
import binascii
import datetime
import hashlib
import json
import re


CAPTURE_SCHEMA = "ssmcp-gh-api-capture-v1"
API_VERSION = "2026-03-10"
REPOSITORY = "LIghtJUNction/ssmcp"
DISPATCH_ENDPOINT = (
    "repos/" + REPOSITORY + "/actions/workflows/release.yml/dispatches"
)
API_REPOSITORY_URL = "https://api.github.com/repos/" + REPOSITORY
HTML_REPOSITORY_URL = "https://github.com/" + REPOSITORY

_CAPTURE_FIELDS = {
    "schema", "argv", "stdin_base64", "stdout_base64", "stderr_base64",
    "exit_code", "started_utc", "finished_utc",
}
_NATIVE_FIELDS = {
    "id", "exit_code", "running", "stdout", "stderr", "stdout_eof",
    "stderr_eof", "stdout_cursor", "error",
}
_SHA256 = re.compile(r"[0-9a-f]{64}")
_UTC = re.compile(
    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}"
    r"(?:\.\d{1,6})?(?:Z|\+00:00)"
)
_HTTP_STATUS = re.compile(
    rb"HTTP/(?:1\.0|1\.1|2(?:\.0)?) ([0-9]{3})(?: [\x20-\x7e]*)?"
)
_HEADER_NAME = re.compile(rb"[!#$%&'*+\-.^_\x60|~0-9A-Za-z]+")


class DispatchReceiptError(ValueError):
    """The supplied original-byte evidence does not meet this contract."""


def _require(condition, message):
    if not condition:
        raise DispatchReceiptError(message)


def _json_document(raw, label):
    _require(type(raw) is bytes, label + " must be original bytes")

    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            _require(key not in result, label + " has duplicate JSON keys")
            result[key] = value
        return result

    def reject_constant(_value):
        raise DispatchReceiptError(label + " has a non-finite JSON value")

    try:
        return json.loads(
            raw.decode("utf-8", errors="strict"),
            object_pairs_hook=unique_object,
            parse_constant=reject_constant,
        )
    except DispatchReceiptError:
        raise
    except (UnicodeError, ValueError, RecursionError) as exc:
        raise DispatchReceiptError(label + " is not one UTF-8 JSON document") from exc


def _utf8(value, label):
    _require(type(value) is str, label + " must be a string")
    try:
        return value.encode("utf-8", errors="strict")
    except UnicodeError as exc:
        raise DispatchReceiptError(label + " has invalid Unicode") from exc


def _sha256_matches(raw, expected, label):
    _require(type(raw) is bytes and len(raw) > 0, label + " must be nonempty bytes")
    _require(
        type(expected) is str and _SHA256.fullmatch(expected) is not None,
        label + " needs an independently supplied lowercase SHA-256",
    )
    actual = hashlib.sha256(raw).hexdigest()
    _require(actual == expected, label + " SHA-256 mismatch")
    return actual


def _base64_bytes(value, label):
    _require(type(value) is str, label + " must be a base64 string")
    try:
        encoded = value.encode("ascii", errors="strict")
        decoded = base64.b64decode(encoded, validate=True)
    except (UnicodeError, ValueError, binascii.Error) as exc:
        raise DispatchReceiptError(label + " is not strict base64") from exc
    _require(
        base64.b64encode(decoded).decode("ascii") == value,
        label + " is not canonical base64",
    )
    return decoded


def _utc_time(value, label):
    _require(
        type(value) is str and _UTC.fullmatch(value) is not None,
        label + " must be an explicit UTC timestamp",
    )
    try:
        parsed = datetime.datetime.fromisoformat(
            value[:-1] + "+00:00" if value.endswith("Z") else value
        )
    except ValueError as exc:
        raise DispatchReceiptError(label + " is not a valid UTC timestamp") from exc
    _require(parsed.utcoffset() == datetime.timedelta(0), label + " is not UTC")
    return parsed


def _same_json_value(left, right):
    # Python equality alone conflates False with 0 and True with 1.
    options = {"sort_keys": True, "separators": (",", ":"), "ensure_ascii": True}
    return json.dumps(left, **options) == json.dumps(right, **options)


def _parse_argv(argv):
    _require(
        type(argv) is list
        and all(type(item) is str for item in argv)
        and argv[:2] == ["gh", "api"],
        "capture argv must start with gh api",
    )
    options = {}
    endpoint = None
    index = 2
    value_options = {"--method", "--header", "--hostname", "--input"}
    while index < len(argv):
        item = argv[index]
        if item == "--include":
            _require(item not in options, "duplicate --include option")
            options[item] = True
            index += 1
        elif item in value_options:
            _require(item not in options, "duplicate gh api option")
            _require(index + 1 < len(argv), "incomplete gh api option")
            options[item] = argv[index + 1]
            index += 2
        else:
            _require(not item.startswith("-"), "unsupported gh api option")
            _require(endpoint is None, "multiple gh api endpoints")
            endpoint = item
            index += 1
    _require(
        set(options) == value_options | {"--include"},
        "capture argv is missing a required gh api option",
    )
    _require(endpoint == DISPATCH_ENDPOINT, "dispatch endpoint mismatch")
    _require(options["--method"] == "POST", "capture is not a POST dispatch")
    _require(options["--hostname"] == "github.com", "explicit GitHub hostname required")
    _require(options["--input"] == "-", "dispatch body must come from captured stdin")
    header = options["--header"].split(":", 1)
    _require(
        len(header) == 2
        and header[0].lower() == "x-github-api-version"
        and header[1].strip() == API_VERSION,
        "dispatch API version mismatch",
    )
    return {
        "method": "POST",
        "url": "https://api.github.com/" + DISPATCH_ENDPOINT,
        "api_version": API_VERSION,
    }


def _explicit_false(value):
    return value is False or (type(value) is str and value == "false")


def _parse_request_body(stdin):
    body = _json_document(stdin, "captured request body")
    _require(
        type(body) is dict and set(body) == {"ref", "inputs"},
        "dispatch body must contain exactly ref and inputs",
    )
    _require(body["ref"] == "main", "dispatch workflow ref must be main")
    inputs = body["inputs"]
    _require(
        type(inputs) is dict
        and set(inputs) == {"tag", "build_only", "deploy_server"},
        "dispatch inputs must explicitly contain tag, build_only and deploy_server",
    )
    _require(
        type(inputs["tag"]) is str and len(inputs["tag"]) > 0,
        "dispatch tag must be a nonempty string",
    )
    _require(
        _explicit_false(inputs["build_only"]),
        "build_only must be explicit false",
    )
    _require(
        _explicit_false(inputs["deploy_server"]),
        "deploy_server must be explicit false",
    )
    return body


def _parse_http_response(stdout):
    _require(type(stdout) is bytes and len(stdout) > 0, "captured stdout is empty")
    status_line, separator, remainder = stdout.partition(b"\n")
    _require(separator == b"\n", "missing gh --include status line")
    if status_line.endswith(b"\r"):
        status_line = status_line[:-1]
    match = _HTTP_STATUS.fullmatch(status_line)
    _require(match is not None, "invalid gh --include HTTP status line")
    status = int(match.group(1))
    _require(status == 200, "dispatch did not return HTTP 200")

    # Observed gh output uses LF for the status line and CRLF for headers.
    # Accept that representation and uniform LF/CRLF without changing body bytes.
    separators = [
        (position, marker)
        for marker in (b"\r\n\r\n", b"\n\n")
        for position in [remainder.find(marker)]
        if position >= 0
    ]
    _require(len(separators) > 0, "missing HTTP header/body boundary")
    boundary, marker = min(separators, key=lambda item: item[0])
    raw_headers = remainder[:boundary].replace(b"\r\n", b"\n")
    _require(b"\r" not in raw_headers, "bare CR in HTTP headers")
    headers = {}
    for line in raw_headers.split(b"\n"):
        name, colon, value = line.partition(b":")
        _require(
            colon == b":" and _HEADER_NAME.fullmatch(name) is not None,
            "invalid HTTP header",
        )
        _require(
            all(byte == 9 or 32 <= byte <= 126 or byte >= 128 for byte in value),
            "control character in HTTP header",
        )
        headers.setdefault(name.lower(), []).append(value.strip(b" \t"))

    selected = headers.get(b"x-github-api-version-selected")
    if selected is not None:
        _require(
            selected == [API_VERSION.encode("ascii")],
            "server-selected API version mismatch",
        )
    content_types = headers.get(b"content-type")
    if content_types is not None:
        _require(len(content_types) == 1, "ambiguous HTTP content type")
        media_type = content_types[0].split(b";", 1)[0].strip().lower()
        _require(
            media_type == b"application/json"
            or (media_type.startswith(b"application/") and media_type.endswith(b"+json")),
            "dispatch response content type is not JSON",
        )

    body = _json_document(
        remainder[boundary + len(marker):], "captured dispatch response"
    )
    _require(type(body) is dict, "dispatch response must be a JSON object")
    run_id = body.get("workflow_run_id")
    _require(type(run_id) is int and run_id > 0, "dispatch response lacks a real run ID")
    _require(
        body.get("run_url") == API_REPOSITORY_URL + "/actions/runs/" + str(run_id),
        "dispatch response run_url mismatch",
    )
    _require(
        body.get("html_url") == HTML_REPOSITORY_URL + "/actions/runs/" + str(run_id),
        "dispatch response html_url mismatch",
    )
    return {"status": status, "body": body}


def parse_dispatch_capture(
    capture_bytes,
    *,
    expected_capture_sha256,
    native_execution_receipt_bytes,
    expected_native_execution_receipt_sha256,
    expected_native_execution_binding,
):
    """Return request/response facts derived from recorded original bytes.

    The caller must obtain both expected hashes, native_job_id and capture_path
    from an independently reviewed original tool observation.  Supplying two
    self-authored JSON files and their hashes does not establish authenticity.
    Later run/attempt, expected tag, workflow SHA, jobs and release binding remain
    the responsibility of the main verifier.
    """
    capture_sha = _sha256_matches(
        capture_bytes, expected_capture_sha256, "capture"
    )
    _sha256_matches(
        native_execution_receipt_bytes,
        expected_native_execution_receipt_sha256,
        "native execution receipt",
    )
    binding = expected_native_execution_binding
    _require(
        type(binding) is dict
        and set(binding) == {"native_job_id", "capture_path"}
        and all(type(value) is str and len(value) > 0 for value in binding.values()),
        "independently reviewed native job/path binding is required",
    )
    native = _json_document(native_execution_receipt_bytes, "native execution receipt")
    _require(
        type(native) is dict and _NATIVE_FIELDS <= set(native),
        "native execution receipt fields are incomplete",
    )
    _require(native["id"] == binding["native_job_id"], "native job binding mismatch")
    _require(
        type(native["exit_code"]) is int and native["exit_code"] == 0,
        "native capture command did not exit successfully",
    )
    _require(native["running"] is False, "native capture command is still running")
    _require(
        native["stdout_eof"] is True and native["stderr_eof"] is True,
        "native capture receipt does not contain both complete streams",
    )
    _require(
        type(native["stdout_cursor"]) is int and native["stdout_cursor"] >= 0,
        "native stdout cursor is invalid",
    )
    _require(type(native["stderr"]) is str, "native stderr is not recorded")
    _require(native["error"] is None, "native execution receipt contains an error")
    for flag in ("truncated", "stdout_truncated", "stderr_truncated"):
        if flag in native:
            _require(native[flag] is False, "native execution receipt is truncated")

    native_stdout = _utf8(native["stdout"], "native stdout")
    _require(
        native["stdout_cursor"] == len(native_stdout),
        "native stdout must contain the complete stream from byte zero",
    )
    envelope = _json_document(native_stdout, "native stdout")
    _require(
        type(envelope) is dict and set(envelope) == {"file", "capture"},
        "native stdout must be the complete file/capture envelope",
    )
    file_binding = envelope["file"]
    _require(
        type(file_binding) is dict and set(file_binding) == {"path", "bytes", "sha256"},
        "native capture file descriptor is invalid",
    )
    _require(file_binding["path"] == binding["capture_path"], "native capture path mismatch")
    _require(
        type(file_binding["bytes"]) is int and file_binding["bytes"] == len(capture_bytes),
        "native capture byte length mismatch",
    )
    _require(file_binding["sha256"] == capture_sha, "native capture SHA-256 mismatch")

    capture = _json_document(capture_bytes, "capture")
    _require(
        type(capture) is dict and set(capture) == _CAPTURE_FIELDS,
        "capture must contain the original collector fields only",
    )
    _require(capture["schema"] == CAPTURE_SCHEMA, "capture schema mismatch")
    _require(
        _same_json_value(envelope["capture"], capture),
        "native envelope capture differs from the original capture",
    )
    _require(
        type(capture["exit_code"]) is int and capture["exit_code"] == 0,
        "captured gh command did not exit successfully",
    )
    started = _utc_time(capture["started_utc"], "capture start")
    finished = _utc_time(capture["finished_utc"], "capture finish")
    _require(finished >= started, "capture timestamps are reversed")

    request = _parse_argv(capture["argv"])
    stdin = _base64_bytes(capture["stdin_base64"], "captured stdin")
    stdout = _base64_bytes(capture["stdout_base64"], "captured stdout")
    _base64_bytes(capture["stderr_base64"], "captured stderr")
    request["body"] = _parse_request_body(stdin)
    response = _parse_http_response(stdout)
    return {"request": request, "response": response}
