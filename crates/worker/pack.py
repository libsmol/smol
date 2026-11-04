"""Packs a CPython standard library folder into one zip of sources and bytecode.

Usage: python3.14 -I pack.py LIBDIR OUT.zip
The bytecode only loads in the CPython minor version that runs this.
"""
import os
import py_compile
import sys
import tempfile
import zipfile

SKIPPED_DIRS = {"test", "tests", "idlelib", "tkinter", "turtledemo", "__pycache__", "site-packages", "lib-dynload"}
SKIPPED_FILES = {"turtle.py"}

libdir, out = sys.argv[1], sys.argv[2]
with tempfile.TemporaryDirectory() as scratch, zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
    compiled = os.path.join(scratch, "compiled.pyc")
    for root, dirs, files in os.walk(libdir):
        relative = os.path.relpath(root, libdir)
        dirs[:] = sorted(name for name in dirs if name not in SKIPPED_DIRS and not name.startswith("config-"))
        for name in sorted(files):
            if name in SKIPPED_FILES or name.endswith((".pyc", ".so", ".a")):
                continue
            path = os.path.join(root, name)
            entry = os.path.normpath(os.path.join(relative, name)).replace(os.sep, "/")
            archive.write(path, entry)
            if not name.endswith(".py"):
                continue
            # Unchecked, since no source in the zip ever changes
            py_compile.compile(path, cfile=compiled, dfile=entry, doraise=True,
                               invalidation_mode=py_compile.PycInvalidationMode.UNCHECKED_HASH)
            # Stored, since every import reads one and deflate costs more time than it saves space
            archive.write(compiled, entry + "c", compress_type=zipfile.ZIP_STORED)
