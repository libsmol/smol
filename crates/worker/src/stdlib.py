def _install():
    import zipimport
    import _stdlib

    path = _stdlib.path()
    original = zipimport.zipimporter.get_resource_reader
    Reader = None

    # Imported on the first read, since zipfile and importlib.resources would slow every start
    def reader_class():
        import contextlib
        import os
        import pathlib
        import shutil
        import tempfile
        import zipfile
        from importlib.resources import _common
        from importlib.resources.readers import ZipReader

        # zipfile opens an archive with plain open(), which reads the executable instead
        archive = zipfile.ZipFile(_stdlib.open())

        class Entry(zipfile.Path):
            pass

        def copy(path, target):
            if path.is_dir():
                os.mkdir(target)
                for child in path.iterdir():
                    copy(child, os.path.join(target, child.name))
                return
            with path.open("rb") as source, open(target, "wb") as copied:
                shutil.copyfileobj(source, copied)

        # The copy keeps its name, which pip reads a wheel's version from
        @_common.as_file.register(Entry)
        @contextlib.contextmanager
        def as_file(path):
            with tempfile.TemporaryDirectory() as folder:
                target = os.path.join(folder, path.name)
                copy(path, target)
                yield pathlib.Path(target)

        class Reader(ZipReader):
            def files(self):
                return Entry(archive, self.prefix)

        return Reader

    def reader(self, fullname):
        nonlocal Reader
        if self.archive != path:
            return original(self, fullname)
        if Reader is None:
            Reader = reader_class()
        return Reader(self, fullname)

    # venv copies its activation scripts from the folder of its file, which is inside the executable
    def setup_scripts(self, context):
        from importlib import resources

        scripts = resources.files("venv") / "scripts"
        with resources.as_file(scripts) as folder:
            self.install_scripts(context, str(folder))

    # On Windows a venv's python.exe is this program too, which relays to the interpreter its pyvenv.cfg names
    def setup_python(self, context):
        import os
        import shutil
        import sys

        if os.path.exists(context.env_exe):
            return
        try:
            os.link(sys._base_executable, context.env_exe)
        except OSError:
            # Kept current by its size and the time it was written, which copy2 keeps
            shutil.copy2(sys._base_executable, context.env_exe)

    executed = zipimport.zipimporter.exec_module

    def exec_module(self, module):
        executed(self, module)
        if self.archive != path or module.__name__ != "venv":
            return
        builder = module.EnvBuilder
        builder.setup_scripts = setup_scripts
        if module.os.name != "nt":
            return

        # venv names the interpreter after sys._base_executable, which is python314.dll here
        directories = builder.ensure_directories

        def ensure_directories(self, env_dir):
            context = directories(self, env_dir)
            context.python_exe = "python.exe"
            context.env_exe = module.os.path.join(context.bin_path, "python.exe")
            context.env_exec_cmd = context.env_exe
            return context

        builder.ensure_directories = ensure_directories
        builder.setup_python = setup_python

    zipimport.zipimporter.get_resource_reader = reader
    zipimport.zipimporter.exec_module = exec_module


_install()
del _install
