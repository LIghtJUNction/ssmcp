"""Read the disposable ConPTY child's console modes without submitting input."""
import ctypes as C
import json
import sys
from ctypes import wintypes as W


def sample(pid):
    k = C.WinDLL("kernel32", use_last_error=True)

    def api(name, result, *args):
        fn = getattr(k, name)
        fn.restype, fn.argtypes = result, args
        return fn

    close = api("CloseHandle", W.BOOL, W.HANDLE)
    process = api("OpenProcess", W.HANDLE, W.DWORD, W.BOOL, W.DWORD)(0x1000, False, pid)
    data = {}
    if process:
        try:
            machine, native = W.WORD(), W.WORD()
            if api("IsWow64Process2", W.BOOL, W.HANDLE, C.POINTER(W.WORD), C.POINTER(W.WORD))(
                process, C.byref(machine), C.byref(native)
            ):
                data.update(process_machine=machine.value, native_machine=native.value)
        finally:
            close(process)
    # Disconnect only this helper from its inherited console.
    api("FreeConsole", W.BOOL)()
    if not api("AttachConsole", W.BOOL, W.DWORD)(pid):
        return {**data, "attach_error": C.get_last_error()}
    open_device = api("CreateFileW", W.HANDLE, W.LPCWSTR, W.DWORD, W.DWORD,
                      W.LPVOID, W.DWORD, W.DWORD, W.HANDLE)
    mode = api("GetConsoleMode", W.BOOL, W.HANDLE, C.POINTER(W.DWORD))
    try:
        for field, device in [("input", "CONIN$"), ("output", "CONOUT$")]:
            handle = open_device(device, 0x80000000, 3, None, 3, 0, None)
            if handle == C.c_void_p(-1).value:
                data[field + "_error"] = C.get_last_error()
                continue
            try:
                value = W.DWORD()
                if mode(handle, C.byref(value)):
                    data[field] = value.value
                else:
                    data[field + "_error"] = C.get_last_error()
            finally:
                close(handle)
    finally:
        api("FreeConsole", W.BOOL)()
    return data


if __name__ == "__main__":
    # FreeConsole can invalidate CRT console descriptors. Write a private sidecar.
    from pathlib import Path
    Path(sys.argv[2]).write_text(json.dumps(sample(int(sys.argv[1]))), encoding="utf-8")
