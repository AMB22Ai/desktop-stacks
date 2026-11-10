#!/usr/bin/env python3
"""Fails if the exe links to a DLL that Windows would look for next to the exe,
or if the exe carries no built-in manifest.

Windows takes "KnownDLLs" and API sets (api-ms-win-*) from System32 no matter what sits
in the exe's folder. Any other DLL in the import table could be replaced by a
file dropped next to the exe (DLL planting). Such DLLs must be loaded at run time through
src/harden.rs instead. Without a manifest inside the exe, a "<exe>.manifest" file placed
next to it could redirect even System32 DLLs (see app.manifest). Run after every build:

usage: python3 tools/check_imports.py path/to/desktop-stacks.exe
"""
import struct
import sys

# Subset of HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\KnownDLLs (Windows 10 and 11)
# plus ntdll and the DLLs those depend on, which Windows protects the same way.
ALLOWED = {
    "kernel32.dll", "ntdll.dll", "user32.dll", "gdi32.dll", "advapi32.dll", "ole32.dll",
    "oleaut32.dll", "shell32.dll", "msvcrt.dll",
    # used by the Rust standard library; checked with a fake copy next to the exe (security test)
    "bcryptprimitives.dll",
    # API sets, by exact name: Windows resolves these through its own table, not by file search
    "api-ms-win-core-synch-l1-2-0.dll", "api-ms-win-shcore-scaling-l1-1-1.dll",
}


def parse(path):
    """Returns (file bytes, offset of the optional header, function RVA -> file offset)."""
    data = open(path, "rb").read()
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise SystemExit("not a PE file")
    nsec = struct.unpack_from("<H", data, pe + 6)[0]
    opt = pe + 24
    if struct.unpack_from("<H", data, opt)[0] != 0x20B:
        raise SystemExit("not a 64-bit PE file")
    opt_size = struct.unpack_from("<H", data, pe + 20)[0]
    secs = []
    for i in range(nsec):
        o = opt + opt_size + 40 * i
        vsize, vaddr, rsize, raw = struct.unpack_from("<IIII", data, o + 8)
        secs.append((vaddr, max(vsize, rsize), raw))

    def off(rva):
        for vaddr, size, raw in secs:
            if vaddr <= rva < vaddr + size:
                return rva - vaddr + raw
        raise SystemExit("bad RVA in the exe")

    return data, opt, off


def imports(data, opt, off):
    imp_rva = struct.unpack_from("<I", data, opt + 112 + 8 * 1)[0]
    delay_rva = struct.unpack_from("<I", data, opt + 112 + 8 * 13)[0]
    names = []
    # (table RVA, size of one entry, where the DLL name RVA sits): normal and delay-load imports
    for table, step, name_at in ((imp_rva, 20, 12), (delay_rva, 32, 4)):
        if table == 0:
            continue
        o = off(table)
        while True:
            name_rva = struct.unpack_from("<I", data, o + name_at)[0]
            if name_rva == 0:
                break
            n = off(name_rva)
            names.append(data[n:data.index(b"\0", n)].decode("ascii").lower())
            o += step
    if not names:
        raise SystemExit("no import table found")
    return sorted(set(names))


def manifest(data, opt, off):
    """Returns the manifest Windows will use: resource type 24 (RT_MANIFEST), id 1.
    None if the exe has no such resource. The same text stored anywhere else in the
    file (another resource type, a string) does not count, because Windows ignores it."""
    res_rva = struct.unpack_from("<I", data, opt + 112 + 8 * 2)[0]
    if res_rva == 0:
        return None
    base = off(res_rva)

    def child(directory, wanted):
        # a directory: 16 byte header, then entries of (name or id, offset); the top
        # bit of the first field marks a name, the top bit of the second a subdirectory
        named, ids = struct.unpack_from("<HH", data, base + directory + 12)
        if named + ids > 4096:
            return None
        for i in range(named + ids):
            key, target = struct.unpack_from("<II", data, base + directory + 16 + 8 * i)
            if wanted is None or key == wanted:
                return target
        return None

    try:
        by_type = child(0, 24)
        if by_type is None or not by_type & 0x80000000:
            return None
        by_id = child(by_type & 0x7FFFFFFF, 1)
        if by_id is None or not by_id & 0x80000000:
            return None
        entry = child(by_id & 0x7FFFFFFF, None)  # any language
        if entry is None or entry & 0x80000000:
            return None
        rva, size = struct.unpack_from("<II", data, base + entry)
        start = off(rva)
        return data[start:start + size]
    except struct.error:
        return None


def main(path):
    data, opt, off = parse(path)
    found = imports(data, opt, off)
    bad = [n for n in found if n not in ALLOWED]
    for n in found:
        print(("  ok   " if n not in bad else "  BAD  ") + n)
    if bad:
        print("FAIL: load these at run time through src/harden.rs: " + ", ".join(bad))
        return 1
    text = manifest(data, opt, off)
    if text is None or b"requestedExecutionLevel" not in text or b"<assembly" not in text:
        print("FAIL: no manifest inside the exe (app.manifest was not embedded as resource type 24, id 1)")
        return 1
    print("imports are limited to protected system DLLs, manifest is built in")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]) if len(sys.argv) == 2 else 2)
